//! Real sockets/FDs only. These issue no Root/SDK/source/exit authority.
use super::*;
#[test]fn disposal_distinguishes_pending_half_close_and_all_peer_aliases_closed(){let(a,b)=runtime_socket_pair().unwrap();let duplicate=b.try_clone().unwrap();let a=RuntimeEndpoint::capture(a).unwrap();let b=RuntimeEndpoint::capture(b).unwrap();let mut packet=None;
    assert_eq!(a.receive_disposal_retaining(&mut packet).unwrap(),RuntimeDisposalRead::Pending);assert!(b.send_data(b"known-private-closure-data",&[]).unwrap());b.shutdown_disposal_write().unwrap();
    assert_eq!(a.receive_disposal_retaining(&mut packet).unwrap(),RuntimeDisposalRead::Packet);assert_eq!(packet.take().unwrap().bytes,b"known-private-closure-data");
    assert_eq!(a.receive_disposal_retaining(&mut packet).unwrap(),RuntimeDisposalRead::ReadClosed{peer_fully_closed:false});drop(b);
    assert_eq!(a.receive_disposal_retaining(&mut packet).unwrap(),RuntimeDisposalRead::ReadClosed{peer_fully_closed:false});drop(duplicate);
    assert_eq!(a.receive_disposal_retaining(&mut packet).unwrap(),RuntimeDisposalRead::ReadClosed{peer_fully_closed:true});}
#[test]fn disposal_invalid_descriptor_count_stays_in_original_pending_owner(){let(a,b)=runtime_socket_pair().unwrap();let a=RuntimeEndpoint::capture(a).unwrap();let harmless=File::open("/dev/null").unwrap();
    let fds:Vec<_>=(0..MAX_RUNTIME_FDS+1).map(|_|harmless.try_clone().unwrap()).collect();let bytes=b"overbound-data-only";let mut iov=libc::iovec{iov_base:bytes.as_ptr().cast_mut().cast(),iov_len:bytes.len()};let mut control=[0usize;64];let mut msg:libc::msghdr=unsafe{std::mem::zeroed()};msg.msg_iov=&mut iov;msg.msg_iovlen=1;msg.msg_control=control.as_mut_ptr().cast();msg.msg_controllen=unsafe{libc::CMSG_SPACE((fds.len()*4)as _)}as usize;
    unsafe{let h=libc::CMSG_FIRSTHDR(&msg);(*h).cmsg_level=libc::SOL_SOCKET;(*h).cmsg_type=libc::SCM_RIGHTS;(*h).cmsg_len=libc::CMSG_LEN((fds.len()*4)as _)as usize;for(i,fd)in fds.iter().enumerate(){std::ptr::write_unaligned(libc::CMSG_DATA(h).cast::<i32>().add(i),fd.as_raw_fd());}}
    assert_eq!(unsafe{libc::sendmsg(b.as_raw_fd(),&msg,libc::MSG_NOSIGNAL)},bytes.len()as isize);let mut pending=None;assert!(a.receive_disposal_retaining(&mut pending).is_err());
    assert_eq!(pending.as_ref().unwrap().fds.len(),MAX_RUNTIME_FDS+1);for fd in &pending.unwrap().fds{assert!(unsafe{libc::fcntl(fd.as_raw_fd(),libc::F_GETFD)}>=0);}}
