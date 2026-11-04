use super::*;

const ROLE: &str = "RUDDER_WRITER_FD_TEST_ROLE";
const CHANNEL: &str = "RUDDER_WRITER_FD_TEST_CHANNEL";
const WRITER: &str = "RUDDER_WRITER_FD_TEST_WRITER";
const DEVICE: &str = "RUDDER_WRITER_FD_TEST_DEVICE";
const INODE: &str = "RUDDER_WRITER_FD_TEST_INODE";

struct ProcessFixture {
    child: std::process::Child,
    channel: UnixStream,
    reaped: bool,
}
impl ProcessFixture {
    fn ready(&mut self) -> u8 {
        let mut byte = [0]; self.channel.read_exact(&mut byte).unwrap(); byte[0]
    }
    fn finish(mut self) {
        self.channel.write_all(b"x").unwrap();
        let status = self.child.wait().unwrap(); self.reaped = true;
        assert!(status.success(), "fixture process failed: {status}");
    }
}
impl Drop for ProcessFixture {
    fn drop(&mut self) {
        if !self.reaped {
            if self.child.try_wait().ok().flatten().is_none() { let _ = self.child.kill(); }
            let _ = self.child.wait();
        }
    }
}
fn spawn_fixture(role: &str, writer: Option<(RawFd, u64, u64)>, designated: bool) -> ProcessFixture {
    let (channel, child_channel) = UnixStream::pair().unwrap();
    channel.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    channel.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
    let channel_fd = child_channel.as_raw_fd();
    let (writer_fd, device, inode) = writer.unwrap_or((-1, 0, 0));
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args(["--ignored", "--nocapture", "--test-threads=1", "writer_fd_process_fixture"])
        .env_clear().env(ROLE, role).env(CHANNEL, channel_fd.to_string())
        .env(WRITER, writer_fd.to_string()).env(DEVICE, device.to_string()).env(INODE, inode.to_string())
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::inherit());
    unsafe {
        // Even the unrelated exec uses pre_exec, matching concurrent service
        // CLIs and avoiding platform posix_spawn close-default differences.
        command.pre_exec(move || {
            clear_cloexec_for_child(channel_fd)?;
            if designated { clear_cloexec_for_child(writer_fd)?; }
            Ok(())
        });
    }
    let child = command.spawn().unwrap(); drop(child_channel);
    ProcessFixture { child, channel, reaped: false }
}
fn writer_is_present(fd: RawFd, device: u64, inode: u64) -> bool {
    let mut actual: libc::stat = unsafe { std::mem::zeroed() };
    fd >= 3 && unsafe { libc::fstat(fd, &mut actual) } == 0
        && actual.st_mode & libc::S_IFMT == libc::S_IFREG
        && actual.st_dev as u64 == device && actual.st_ino as u64 == inode
}
fn assert_busy(path: &Path) {
    let error = lock_file(path).unwrap().try_lock_exclusive().unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
}
fn assert_available(path: &Path) {
    let probe = lock_file(path).unwrap(); probe.try_lock_exclusive().unwrap(); FileExt::unlock(&probe).unwrap();
}
fn scenario(legacy: bool, descendant: bool) {
    let directory = tempfile::tempdir().unwrap(); let path = directory.path().join("writer.lock");
    let writer = lock_file(&path).unwrap(); writer.try_lock_exclusive().unwrap();
    let duplicate = if legacy {
        let fd = unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_DUPFD, 3) };
        assert!(fd >= 3); unsafe { OwnedFd::from_raw_fd(fd) }
    } else { duplicate_writer_for_child(&writer).unwrap() };
    let metadata = writer.metadata().unwrap();
    let identity = (duplicate.as_raw_fd(), metadata.dev(), metadata.ino());
    let mut designated = spawn_fixture(if descendant { "delegate" } else { "hold" }, Some(identity), true);
    assert_eq!(designated.ready(), if descendant { b'D' } else { b'H' });
    let flags = unsafe { libc::fcntl(duplicate.as_raw_fd(), libc::F_GETFD) };
    assert!(flags >= 0);
    assert_eq!(flags & libc::FD_CLOEXEC != 0, !legacy,
        "designated pre_exec must not clear the parent's descriptor flag");
    let mut unrelated = spawn_fixture("hold", Some(identity), false);
    assert_eq!(unrelated.ready(), if legacy { b'H' } else { b'N' });
    drop(duplicate); drop(writer);
    assert_busy(&path); // Designated exec or its actual synchronous descendant owns it.
    designated.finish();
    assert!(unrelated.child.try_wait().unwrap().is_none());
    if legacy {
        assert_busy(&path); // Actual old-style leak: target reaped, unrelated exec still holds it.
        unrelated.finish(); assert_available(&path);
    } else {
        assert_available(&path); // Unrelated exec is still alive but cannot retain this writer.
        unrelated.finish();
    }
}
fn driver(role: &str) {
    // Isolate the deliberately inheritable legacy control from parallel tests'
    // process spawns. The control FD exists only in this single-fixture child.
    let mut process = spawn_fixture(role, None, false);
    assert_eq!(process.ready(), b'R'); process.finish();
}

#[test]
fn writer_fd_cloexec_excludes_unrelated_exec_after_designated_child_exit() { driver("driver-safe"); }

#[test]
fn writer_fd_legacy_control_reproduces_unrelated_exec_retention() { driver("driver-legacy"); }

#[test]
fn writer_fd_designated_synchronous_descendant_preserves_the_canonical_fence() { driver("driver-descendant"); }

#[test]
#[ignore = "invoked only by the actual-process writer FD regressions"]
fn writer_fd_process_fixture() {
    let role = std::env::var(ROLE).unwrap();
    let channel_fd: RawFd = std::env::var(CHANNEL).unwrap().parse().unwrap();
    let mut channel = unsafe { UnixStream::from_raw_fd(channel_fd) };
    channel.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    channel.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
    // Our control channel is not a synchronous writer descendant's resource.
    let flags = unsafe { libc::fcntl(channel_fd, libc::F_GETFD) };
    assert!(flags >= 0 && unsafe { libc::fcntl(channel_fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } == 0);
    let writer_fd: RawFd = std::env::var(WRITER).unwrap().parse().unwrap();
    let device = std::env::var(DEVICE).unwrap().parse().unwrap();
    let inode = std::env::var(INODE).unwrap().parse().unwrap();
    let mut descendant = None;
    let ready = match role.as_str() {
        "driver-safe" => { scenario(false, false); b'R' }
        "driver-legacy" => { scenario(true, false); b'R' }
        "driver-descendant" => { scenario(false, true); b'R' }
        "delegate" => {
            assert!(writer_is_present(writer_fd, device, inode));
            let mut process = spawn_fixture("hold", Some((writer_fd, device, inode)), false);
            assert_eq!(process.ready(), b'H', "synchronous exec must retain the writer");
            assert_eq!(unsafe { libc::close(writer_fd) }, 0);
            // Only the actual descendant now holds this open-file lock. The
            // designated process waits for and reaps it before returning.
            descendant = Some(process); b'D'
        }
        "hold" => if writer_is_present(writer_fd, device, inode) { b'H' } else { b'N' },
        _ => panic!("unknown writer FD fixture role"),
    };
    channel.write_all(&[ready]).unwrap();
    let mut exit = [0]; channel.read_exact(&mut exit).unwrap(); assert_eq!(exit, *b"x");
    if let Some(process) = descendant { process.finish(); }
    // Inherited writer descriptors intentionally remain open until this
    // process exits, exactly as the managed CLI's retained canonical fence.
}
