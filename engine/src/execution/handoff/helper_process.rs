//! Parent-side custody of the actual managed helper, never a serialized PID grant.
use super::*;
use std::sync::atomic::{AtomicU8, Ordering};
const PENDING: u8 = 0;
const CURRENT: u8 = 1;
const REVOKED: u8 = 2;

/// Minted only by the owned managed handoff. Descriptor transfer proves no
/// helper validation, source admission, backend readiness or execution success.
/// Keep this witness on the trusted parent side, outside guest projections.
pub struct HelperProcessWitness {
    lifetime: OwnedFd,
    pid: u32,
    uid: u32,
    binding: ManagedExecutionBinding,
    deadline: Instant,
    state: AtomicU8,
}
impl HelperProcessWitness {
    pub(super) fn pending(
        lifetime: OwnedFd,
        pid: u32,
        uid: u32,
        binding: ManagedExecutionBinding,
    ) -> Self {
        Self {
            lifetime,
            pid,
            uid,
            binding,
            deadline: Instant::now() + Duration::from_secs(10),
            state: AtomicU8::new(PENDING),
        }
    }
    pub(super) fn transferred(&self) -> Result<()> {
        let live = match exited(&self.lifetime) {
            Ok(exited) => !exited,
            Err(error) => {
                self.revoke();
                return Err(error);
            }
        };
        if Instant::now() >= self.deadline || !live {
            self.revoke();
            bail!("managed helper handoff deadline/lifetime ended");
        }
        self.state
            .compare_exchange(PENDING, CURRENT, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| anyhow::anyhow!("managed helper custody already retired"))?;
        self.check_current()
    }
    pub(super) fn revoke(&self) {
        self.state.store(REVOKED, Ordering::Release);
    }
    pub(super) fn control_finished(&self, result: &Result<()>) {
        // An accepted receipt remains provisional. The still-live helper can
        // query its source authority again before it actually exits.
        if result.is_err() {
            self.revoke();
        }
    }
    pub fn check_current(&self) -> Result<()> {
        let state = self.state.load(Ordering::Acquire);
        if state == PENDING && Instant::now() >= self.deadline {
            self.revoke();
        }
        let live = match exited(&self.lifetime) {
            Ok(exited) => !exited,
            Err(error) => {
                self.revoke();
                return Err(error);
            }
        };
        if !live {
            self.revoke();
        }
        ensure!(
            live && self.state.load(Ordering::Acquire) == CURRENT,
            "managed helper custody is not current"
        );
        Ok(())
    }
    pub fn verify_binding(&self, binding: &ManagedExecutionBinding) -> Result<()> {
        self.check_current()?;
        ensure!(
            self.binding == *binding,
            "managed helper belongs to another sealed binding"
        );
        self.check_current()
    }
    pub fn verify_peer(&self, stream: &UnixStream) -> Result<()> {
        self.check_current()?;
        let actual = peer(stream)?;
        ensure!(
            actual.pid == self.pid as i32 && actual.uid == self.uid,
            "source socket does not belong to the helper-owned process"
        );
        self.check_current()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Child, Command, Stdio};

    const PAUSED_HELPER_SOCKET: &str = "RUDDER_HELPER_CUSTODY_TEST_SOCKET";

    struct PausedHelper(Child);
    impl PausedHelper {
        fn finish(&mut self) {
            self.0.stdin.take().unwrap().write_all(&[1]).unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if let Some(status) = self.0.try_wait().unwrap() {
                    assert!(status.success(), "paused helper failed: {status}");
                    break;
                }
                assert!(Instant::now() < deadline, "paused helper exit deadline");
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    }
    impl Drop for PausedHelper {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn paused_helper_child() {
        let Some(socket) = std::env::var_os(PAUSED_HELPER_SOCKET) else {
            return;
        };
        let mut source = UnixStream::connect(socket).unwrap();
        // This kernel-lifetime fixture marks the post-receipt boundary only;
        // admission of a complete V4 receipt needs the separate qualification.
        source.write_all(b"receipt").unwrap();
        let mut release = [0];
        std::io::stdin().read_exact(&mut release).unwrap();
        assert_eq!(release, [1]);
    }

    fn paused_after_receipt() -> (PausedHelper, UnixStream, ManagedExecutionHandle) {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("source.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let child = PausedHelper(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "execution::handoff::helper_process::tests::paused_helper_child",
                    "--nocapture",
                ])
                .env(PAUSED_HELPER_SOCKET, socket)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        let (mut source, _) = loop {
            match listener.accept() {
                Ok(connection) => break connection,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "paused helper connect deadline");
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(error) => panic!("paused helper accept: {error}"),
            }
        };
        source
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut receipt = [0; 7];
        source.read_exact(&mut receipt).unwrap();
        assert_eq!(&receipt, b"receipt");
        let binding = super::super::readiness_tests::binding();
        let helper_process = Arc::new(HelperProcessWitness::pending(
            pidfd(child.0.id()).unwrap(),
            child.0.id(),
            unsafe { libc::geteuid() },
            binding.clone(),
        ));
        helper_process.transferred().unwrap();
        helper_process.control_finished(&Ok(()));
        let (commands, _) = mpsc::sync_channel(1);
        let (_, events) = mpsc::sync_channel(1);
        let handle = ManagedExecutionHandle {
            helper_process,
            binding,
            commands,
            events,
            pending: std::cell::RefCell::new(None),
            stop_v4: std::cell::RefCell::new(None),
            authenticated: std::cell::Cell::new(false),
            closed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        (child, source, handle)
    }

    #[test]
    fn accepted_receipt_keeps_helper_custody_until_kernel_exit() {
        let (mut child, source, handle) = paused_after_receipt();
        let witness = handle.helper_process_witness();
        drop(handle.helper_process_witness());
        witness.verify_peer(&source).unwrap();
        witness.verify_binding(&handle.binding).unwrap();
        child.finish();
        assert!(witness.check_current().is_err());
        assert!(witness.verify_peer(&source).is_err());
        assert!(witness.transferred().is_err());
    }

    #[test]
    fn handle_retirement_revokes_retained_custody_while_helper_is_alive() {
        let (mut child, source, handle) = paused_after_receipt();
        let witness = handle.helper_process_witness();
        let closed = handle.closed.clone();
        witness.verify_peer(&source).unwrap();
        drop(handle);
        assert!(closed.load(Ordering::Acquire));
        assert!(!exited(&witness.lifetime).unwrap());
        assert!(witness.verify_peer(&source).is_err());
        witness.control_finished(&Ok(()));
        assert!(witness.transferred().is_err());
        assert!(witness.check_current().is_err());
        child.finish();
    }

    #[test]
    fn failed_control_revokes_custody_absorbingly() {
        let witness = fixture();
        witness.transferred().unwrap();
        witness.control_finished(&Err(anyhow::anyhow!("control failed")));
        witness.control_finished(&Ok(()));
        assert!(witness.check_current().is_err());
        assert!(witness.transferred().is_err());
    }

    fn fixture() -> HelperProcessWitness {
        HelperProcessWitness::pending(
            pidfd(std::process::id()).unwrap(),
            std::process::id(),
            unsafe { libc::geteuid() },
            super::super::readiness_tests::binding(),
        )
    }
    #[test]
    fn pending_and_revoked_custody_cannot_authenticate_a_socket() {
        let witness = fixture();
        let (socket, _other) = UnixStream::pair().unwrap();
        assert!(witness.check_current().is_err());
        assert!(witness.verify_peer(&socket).is_err());
        witness.transferred().unwrap();
        witness.verify_peer(&socket).unwrap();
        witness.verify_binding(&witness.binding).unwrap();
        let mut changed = witness.binding.clone();
        changed.run_epoch += 1;
        assert!(witness.verify_binding(&changed).is_err());
        witness.revoke();
        assert!(witness.check_current().is_err());
        assert!(witness.transferred().is_err());
        assert!(witness.verify_peer(&socket).is_err());
    }
    #[test]
    fn pending_deadline_and_receipt_timeout_are_absorbing() {
        let mut witness = fixture();
        witness.deadline = Instant::now() - Duration::from_millis(1);
        assert!(witness.check_current().is_err());
        assert!(witness.transferred().is_err());
        let witness = Arc::new(fixture());
        witness.transferred().unwrap();
        let (_sender, result) = mpsc::sync_channel(1);
        let receipt = HandoffReceipt {
            helper_process: Some(witness.clone()),
            result,
            completed: std::cell::Cell::new(false),
        };
        assert!(receipt.wait(Duration::from_millis(1)).is_err());
        assert!(witness.check_current().is_err());
        assert!(witness.transferred().is_err());
    }
    #[test]
    fn helper_exit_and_wrong_kernel_peer_cannot_be_replaced_by_another_process() {
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("10")
            .spawn()
            .unwrap();
        let witness = HelperProcessWitness::pending(
            pidfd(child.id()).unwrap(),
            child.id(),
            unsafe { libc::geteuid() },
            super::super::readiness_tests::binding(),
        );
        witness.transferred().unwrap();
        let (wrong_socket, _other) = UnixStream::pair().unwrap();
        assert!(witness.verify_peer(&wrong_socket).is_err());
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(witness.check_current().is_err());
        assert!(witness.transferred().is_err());
        let current = fixture();
        current.transferred().unwrap();
        current.verify_peer(&wrong_socket).unwrap();
        assert!(witness.verify_peer(&wrong_socket).is_err());
    }
}
