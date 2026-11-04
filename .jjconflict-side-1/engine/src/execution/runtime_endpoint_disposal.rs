//! Anonymous socket mechanics, not a grant or process/actor drain. A disposal
//! consumer must authenticate its original owner and retain pending packets.
use super::*;

#[derive(Clone,Copy,Debug,PartialEq,Eq)]
pub enum RuntimeDisposalRead { Pending, Packet, ReadClosed { peer_fully_closed:bool } }
impl RuntimeEndpoint {
    /// Stage ALL actually received descriptors before any packet validation.
    /// EOF is distinguished from EAGAIN and requires kernel half/full hangup.
    pub fn receive_disposal_retaining(&self,out:&mut Option<RuntimePacket>)->Result<RuntimeDisposalRead>{
        ensure!(out.is_none(),"original disposal packet slot occupied");
        let mut bytes=vec![0;MAX_FRAME];let mut control=[0usize;64];let mut iov=libc::iovec{iov_base:bytes.as_mut_ptr().cast(),iov_len:bytes.len()};
        let mut msg:libc::msghdr=unsafe{std::mem::zeroed()};msg.msg_iov=&mut iov;msg.msg_iovlen=1;msg.msg_control=control.as_mut_ptr().cast();msg.msg_controllen=std::mem::size_of_val(&control);
        let n=unsafe{libc::recvmsg(self.fd.as_raw_fd(),&mut msg,libc::MSG_DONTWAIT|libc::MSG_CMSG_CLOEXEC)};
        if n<0{let error=io::Error::last_os_error();if error.kind()==io::ErrorKind::WouldBlock{return Ok(RuntimeDisposalRead::Pending)}return Err(error).context("original disposal receive outcome unknown")}
        let mut fds=Vec::new();let mut credentials=None;let mut valid=true;
        unsafe{let mut h=libc::CMSG_FIRSTHDR(&msg);while !h.is_null(){let base=libc::CMSG_LEN(0)as usize;if (*h).cmsg_len<base{valid=false;break}let size=(*h).cmsg_len-base;
            if (*h).cmsg_level==libc::SOL_SOCKET&&(*h).cmsg_type==libc::SCM_RIGHTS{if size%4!=0{valid=false}for i in 0..size/4{fds.push(OwnedFd::from_raw_fd(std::ptr::read_unaligned(libc::CMSG_DATA(h).cast::<i32>().add(i))))}}
            else if (*h).cmsg_level==libc::SOL_SOCKET&&(*h).cmsg_type==libc::SCM_CREDENTIALS&&size==std::mem::size_of::<libc::ucred>()&&credentials.is_none(){credentials=Some(std::ptr::read_unaligned(libc::CMSG_DATA(h).cast::<libc::ucred>()))}
            else{valid=false}h=libc::CMSG_NXTHDR(&msg,h);}}
        let flags_ok=msg.msg_flags&(libc::MSG_TRUNC|libc::MSG_CTRUNC)==0;
        if n==0&&valid&&flags_ok&&fds.is_empty(){let mut poll=libc::pollfd{fd:self.fd.as_raw_fd(),events:libc::POLLIN|libc::POLLRDHUP|libc::POLLHUP,revents:0};
            ensure!(unsafe{libc::poll(&mut poll,1,0)}>=0,"disposal EOF observation failed");
            if poll.revents&(libc::POLLRDHUP|libc::POLLHUP)!=0{return Ok(RuntimeDisposalRead::ReadClosed{peer_fully_closed:poll.revents&libc::POLLHUP!=0})}}
        bytes.truncate(n as usize);let has_credentials=credentials.is_some();
        *out=Some(RuntimePacket{bytes,fds,credentials:credentials.unwrap_or(libc::ucred{pid:0,uid:0,gid:0})});
        ensure!(n>0&&valid&&flags_ok&&has_credentials&&out.as_ref().unwrap().fds.len()<=MAX_RUNTIME_FDS,"disposal packet invalid/truncated; original descriptors retained");
        Ok(RuntimeDisposalRead::Packet)
    }
    /// The caller latches its actual attempt before this syscall. An error
    /// cannot be treated as EOF or retried with another deadline/endpoint.
    pub fn shutdown_disposal_write(&self)->Result<()>{ensure!(unsafe{libc::shutdown(self.fd.as_raw_fd(),libc::SHUT_WR)}==0,"original disposal write shutdown outcome unknown");Ok(())}
}

#[cfg(test)]
#[path="runtime_endpoint_disposal_tests.rs"]mod tests;
