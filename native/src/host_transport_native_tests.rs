//! DATA/kernel transport tests, never a Root/source/lease issuer.
use super::*;
#[test]
fn serial_is_exact_monotonic_signed_i64_data_not_owner_epoch() {
    assert_eq!(NativeTransport::serial("9007199254740993").unwrap(),9_007_199_254_740_993);
    for value in ["0","01","-1","9223372036854775808","18446744073709551616","1.0"] {assert!(NativeTransport::serial(value).is_err());}
}
#[test]
fn actual_seqpacket_eagain_preserves_original_packet_until_kernel_receives_it() {
    // Ordinary kernel packet custody needs no Root bootstrap authority.
    // Keep the production Root-only pair factory and its checks unchanged.
    use std::os::fd::{FromRawFd,OwnedFd};
    let mut raw=[-1;2];
    assert_eq!(unsafe {libc::socketpair(libc::AF_UNIX,libc::SOCK_SEQPACKET|libc::SOCK_NONBLOCK|libc::SOCK_CLOEXEC,0,raw.as_mut_ptr())},0);
    let (left,right)=unsafe {(OwnedFd::from_raw_fd(raw[0]),OwnedFd::from_raw_fd(raw[1]))};
    let sender=Endpoint::capture(left).unwrap();let receiver=Endpoint::capture(right).unwrap();
    let original=vec![b'x';4096];let mut sent=0;while sent<512 {if !sender.send(&original,None).unwrap(){break;}sent+=1;}assert!(sent>0&&sent<512,"fixture must reach actual bounded EAGAIN");
    let packet=receiver.receive().unwrap().unwrap();assert_eq!(packet.bytes,original);assert!(packet.fds.is_empty());
    // Kernel receipts contain actual credentials; no JSON PID creates them.
    assert_eq!(packet.credentials.pid,std::process::id() as i32);
    let mut drained=1;while receiver.receive().unwrap().is_some(){drained+=1;}assert_eq!(drained,sent);assert!(sender.send(&original,None).unwrap());assert_eq!(receiver.receive().unwrap().unwrap().bytes,original);
}
