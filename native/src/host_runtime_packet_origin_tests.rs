//! Actual SEQPACKET/SCM_CREDENTIALS mechanism regressions. No Root/ready/client
//! owner or successful qualification ACK is synthesized. The fixture floor is
//! the actual test process UID/GID; production validate() always fixes1500.
use super::*;
use std::os::fd::FromRawFd;

fn actual_packet(bytes:&[u8],fd:Option<&OwnedFd>)->storage::Packet {
 let mut pair=[-1;2];assert_eq!(unsafe{libc::socketpair(libc::AF_UNIX,libc::SOCK_SEQPACKET|libc::SOCK_NONBLOCK|libc::SOCK_CLOEXEC,0,pair.as_mut_ptr())},0);
 let send=Endpoint::capture(unsafe{OwnedFd::from_raw_fd(pair[0])}).unwrap();
 let recv=Endpoint::capture(unsafe{OwnedFd::from_raw_fd(pair[1])}).unwrap();
 assert!(send.send(bytes,fd).unwrap());recv.receive().unwrap().unwrap()
}
fn ack()->storage::Packet {actual_packet(br#"{"kind":"runtime_image_qualification_ack_v1","packet":{"fixture":"DATA-only"}}"#,None)}
fn fixture_validate(origin:&mut RuntimePacketOrigin,packet:&storage::Packet)->Result<()> {
 origin.validate_owned(packet,unsafe{libc::geteuid()},unsafe{libc::getegid()})
}
fn pump(packet:&mut Option<storage::Packet>,origin:&mut Option<RuntimePacketOrigin>,instance:&str,slot:&mut Option<storage::Packet>)->Result<()> {
 pump_retained_runtime_ack(packet,origin,instance,&mut None,slot)
}

#[test]
fn rejected_kernel_sender_never_promotes_on_repeated_packet_pumps() {
 let packet=ack();assert_eq!(packet.credentials.pid,std::process::id() as i32);
 let mut origin=RuntimePacketOrigin::staged(std::process::id().checked_add(1).unwrap(),"original");
 assert!(fixture_validate(&mut origin,&packet).is_err());
 let mut packet=Some(packet);let mut origin=Some(origin);let mut slot=None;
 for _ in 0..3 {assert!(pump(&mut packet,&mut origin,"original",&mut slot).is_err());assert!(packet.is_some()&&slot.is_none());}
 assert_eq!(origin.unwrap().validation,RuntimePacketValidation::Rejected);
}

#[test]
fn rejected_retired_instance_cannot_reappear_as_current_or_revalidated_json() {
 let packet=ack();let mut origin=RuntimePacketOrigin::staged(std::process::id(),"retired");fixture_validate(&mut origin,&packet).unwrap();
 let mut packet=Some(packet);let mut origin=Some(origin);let mut slot=None;
 assert!(pump(&mut packet,&mut origin,"current",&mut slot).is_err());
 // Even supplying the old instance again cannot undo rejection.
 assert!(pump(&mut packet,&mut origin,"retired",&mut slot).is_err());
 assert!(fixture_validate(origin.as_mut().unwrap(),packet.as_ref().unwrap()).is_err());
 assert!(packet.is_some()&&slot.is_none());assert_eq!(origin.unwrap().validation,RuntimePacketValidation::Rejected);
}

#[test]
fn held_valid_packet_rechecks_original_instance_after_full_ack_slot_clears() {
 let packet=ack();let mut origin=RuntimePacketOrigin::staged(std::process::id(),"original");fixture_validate(&mut origin,&packet).unwrap();
 let mut packet=Some(packet);let mut origin=Some(origin);let mut slot=Some(ack());
 pump(&mut packet,&mut origin,"original",&mut slot).unwrap();assert!(packet.is_some()&&origin.is_some());
 slot.take();assert!(pump(&mut packet,&mut origin,"replacement",&mut slot).is_err());
 assert!(packet.is_some()&&slot.is_none());assert_eq!(origin.unwrap().validation,RuntimePacketValidation::Rejected);
}

#[test]
fn actual_rejected_ancillary_fd_stays_held_through_repeated_pumps() {
 let fd:OwnedFd=File::open("/dev/null").unwrap().into();let packet=actual_packet(br#"{"kind":"runtime_image_qualification_ack_v1"}"#,Some(&fd));
 assert_eq!(packet.fds.len(),1);let number=packet.fds[0].as_raw_fd();
 let mut origin=RuntimePacketOrigin::staged(std::process::id(),"original");assert!(fixture_validate(&mut origin,&packet).is_err());
 let mut packet=Some(packet);let mut origin=Some(origin);let mut slot=None;
 for _ in 0..3 {assert!(pump(&mut packet,&mut origin,"original",&mut slot).is_err());assert!(unsafe{libc::fcntl(number,libc::F_GETFD)}>=0);}
 assert!(slot.is_none()&&packet.as_ref().unwrap().fds.len()==1);
}

#[test]
fn actual_current_validated_packet_moves_once_without_rebuilding_source() {
 let packet=ack();let mut origin=RuntimePacketOrigin::staged(std::process::id(),"original");fixture_validate(&mut origin,&packet).unwrap();
 let mut packet=Some(packet);let mut origin=Some(origin);let mut slot=None;
 pump(&mut packet,&mut origin,"original",&mut slot).unwrap();
 assert!(packet.is_none()&&origin.is_none()&&slot.is_some());assert_eq!(slot.as_ref().unwrap().credentials.pid,std::process::id() as i32);
 assert!(pump(&mut packet,&mut origin,"original",&mut slot).is_err());
}

#[test]
fn malformed_original_bytes_never_gain_origin_from_later_kind_inspection() {
 let packet=actual_packet(b"not-json",None);let mut origin=RuntimePacketOrigin::staged(std::process::id(),"original");assert!(fixture_validate(&mut origin,&packet).is_err());
 let mut packet=Some(packet);let mut origin=Some(origin);let mut slot=None;
 for _ in 0..3 {assert!(pump(&mut packet,&mut origin,"original",&mut slot).is_err());assert!(packet.is_some()&&slot.is_none());}
}
