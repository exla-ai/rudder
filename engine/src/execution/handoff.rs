//! One-use sealed-spec descriptor transfer after portable-pty has closed all
//! inherited FDs. Socket names are not credentials: kernel peer PID/UID plus
//! held pidfds authenticate both ends. No JSON spec enters argv/env/PTY/disk.
use super::*;
use anyhow::{bail, Context};
use std::{
    io, mem,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        linux::net::SocketAddrExt,
        unix::net::{SocketAddr, UnixListener, UnixStream},
    },
    sync::mpsc,
    time::{Duration, Instant},
};

pub struct ExecutionHandoff {
    listener: UnixListener,
    spec: OwnedFd,
    name: String,
    parent: u32,
    uid: u32,
}
pub struct HandoffReceipt {
    result: mpsc::Receiver<std::result::Result<(), String>>,
    completed: std::cell::Cell<bool>,
}
impl HandoffReceipt {
    /// Reports descriptor transfer only, not helper validation, process launch,
    /// namespace readiness or successful execution. Observe ExecutionRecord.
    pub fn try_result(&self) -> Option<std::result::Result<(), String>> {
        if self.completed.get() {
            return None;
        }
        match self.result.try_recv() {
            Ok(r) => {
                self.completed.set(true);
                Some(r)
            }
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => {
                self.completed.set(true);
                Some(Err(
                    "execution handoff closed without acknowledgement".into()
                ))
            }
        }
    }
    pub fn wait(self, timeout: Duration) -> Result<()> {
        self.result
            .recv_timeout(timeout)
            .context("execution handoff deadline")?
            .map_err(anyhow::Error::msg)
    }
}
impl ExecutionHandoff {
    pub fn new(spec: &ExecutionSpec) -> Result<Self> {
        let fd = super::linux::sealed_spec_fd(spec)?;
        let name = format!("rudder-execution-{}", Uuid::new_v4());
        let address = SocketAddr::from_abstract_name(name.as_bytes())?;
        let listener = UnixListener::bind_addr(&address)?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener,
            spec: fd,
            name,
            parent: std::process::id(),
            uid: unsafe { libc::geteuid() },
        })
    }
    /// Only non-secret routing metadata. Use with the real native helper path.
    pub fn helper_args(&self) -> Vec<String> {
        vec![
            "fabric".into(),
            "execution".into(),
            "helper".into(),
            "--handoff".into(),
            self.name.clone(),
            "--parent-pid".into(),
            self.parent.to_string(),
        ]
    }
    /// Call immediately after TerminalPane owns the actual spawned process.
    pub fn activate(self, child: u32) -> Result<HandoffReceipt> {
        ensure!(child > 1 && child != self.parent, "invalid handoff child");
        let pidfd = pidfd(child)?;
        let (send, result) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let r = (|| -> Result<()> {
                let deadline = Instant::now() + Duration::from_secs(10);
                let mut rejected = 0;
                while Instant::now() < deadline {
                    ensure!(
                        !exited(&pidfd)?,
                        "execution child exited before spec handoff"
                    );
                    match self.listener.accept() {
                        Ok((stream, _)) => {
                            let credentials = peer(&stream)?;
                            if credentials.uid != self.uid || credentials.pid != child as i32 {
                                rejected += 1;
                                ensure!(
                                    rejected <= 32,
                                    "too many rejected execution handoff peers"
                                );
                                continue;
                            }
                            stream.set_write_timeout(Some(Duration::from_secs(2)))?;
                            send_fd(&stream, &self.spec)?;
                            return Ok(());
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5))
                        }
                        Err(e) => return Err(e.into()),
                    }
                }
                bail!("execution child did not receive spec before deadline")
            })();
            drop(self);
            let _ = send.send(r.map_err(|e| e.to_string()));
        });
        Ok(HandoffReceipt {
            result,
            completed: std::cell::Cell::new(false),
        })
    }
}
fn pidfd(pid: u32) -> Result<OwnedFd> {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error()).context("open execution lifetime pidfd");
    };
    Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
}
fn exited(fd: &OwnedFd) -> Result<bool> {
    let mut p = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let result = unsafe { libc::poll(&mut p, 1, 0) };
    if result < 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(result > 0)
}
fn peer(stream: &UnixStream) -> Result<libc::ucred> {
    let mut value = unsafe { mem::zeroed::<libc::ucred>() };
    let mut length = mem::size_of::<libc::ucred>() as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut value as *mut libc::ucred).cast(),
            &mut length,
        )
    } != 0
    {
        return Err(io::Error::last_os_error().into());
    }
    ensure!(
        length as usize == mem::size_of::<libc::ucred>(),
        "invalid handoff peer credentials"
    );
    Ok(value)
}
fn send_fd(stream: &UnixStream, fd: &OwnedFd) -> Result<()> {
    let mut byte = [1u8];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: 1,
    };
    let mut control = [0u64; 4];
    let mut header = unsafe { mem::zeroed::<libc::msghdr>() };
    header.msg_iov = &mut iov;
    header.msg_iovlen = 1;
    header.msg_control = control.as_mut_ptr().cast();
    header.msg_controllen = unsafe { libc::CMSG_SPACE(mem::size_of::<i32>() as u32) } as usize;
    unsafe {
        let c = libc::CMSG_FIRSTHDR(&header);
        (*c).cmsg_level = libc::SOL_SOCKET;
        (*c).cmsg_type = libc::SCM_RIGHTS;
        (*c).cmsg_len = libc::CMSG_LEN(mem::size_of::<i32>() as u32) as usize;
        std::ptr::write_unaligned(libc::CMSG_DATA(c).cast::<i32>(), fd.as_raw_fd());
    }
    ensure!(
        unsafe { libc::sendmsg(stream.as_raw_fd(), &header, libc::MSG_NOSIGNAL) } == 1,
        "sealed execution descriptor transfer failed"
    );
    Ok(())
}
pub fn receive(name: &str, parent: u32) -> Result<ExecutionSpec> {
    let _parent_fence = super::linux::arm_parent(parent)?;
    ensure!(
        name.starts_with("rudder-execution-")
            && name.len() <= 80
            && parent > 1
            && unsafe { libc::getppid() } == parent as i32,
        "execution handoff parent/routing mismatch"
    );
    let lifetime = pidfd(parent)?;
    ensure!(!exited(&lifetime)?, "execution parent already exited");
    let address = SocketAddr::from_abstract_name(name.as_bytes())?;
    let stream = UnixStream::connect_addr(&address)?;
    stream.set_read_timeout(Some(Duration::from_secs(12)))?;
    let cred = peer(&stream)?;
    ensure!(
        cred.uid == unsafe { libc::geteuid() } && cred.pid == parent as i32,
        "execution handoff server is not the owning parent"
    );
    let mut byte = [0u8];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: 1,
    };
    let mut control = [0u64; 4];
    let mut header = unsafe { mem::zeroed::<libc::msghdr>() };
    header.msg_iov = &mut iov;
    header.msg_iovlen = 1;
    header.msg_control = control.as_mut_ptr().cast();
    header.msg_controllen = mem::size_of_val(&control);
    let count = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut header, libc::MSG_CMSG_CLOEXEC) };
    let mut descriptors = Vec::new();
    unsafe {
        let mut c = libc::CMSG_FIRSTHDR(&header);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let n = ((*c).cmsg_len.saturating_sub(libc::CMSG_LEN(0) as usize))
                    / mem::size_of::<i32>();
                for index in 0..n {
                    descriptors.push(OwnedFd::from_raw_fd(std::ptr::read_unaligned(
                        libc::CMSG_DATA(c).cast::<i32>().add(index),
                    )));
                }
            }
            c = libc::CMSG_NXTHDR(&header, c);
        }
    }
    ensure!(
        count == 1
            && byte[0] == 1
            && header.msg_flags & libc::MSG_CTRUNC == 0
            && descriptors.len() == 1,
        "invalid sealed execution handoff frame"
    );
    ensure!(
        !exited(&lifetime)?,
        "execution parent exited during handoff"
    );
    super::linux::read_spec_fd(descriptors.pop().unwrap())
}
