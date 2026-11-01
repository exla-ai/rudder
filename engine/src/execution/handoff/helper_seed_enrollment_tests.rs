//! Actual child/pidfd regressions, written UNRUN. No root enrollment, source
//! authorization, use-barrier release or production activation is claimed.
use super::*;
use std::process::{Child, Command, Stdio};

struct LiveChild(Child);
impl LiveChild {
    fn spawn() -> Self {
        Self(Command::new("/bin/sleep").arg("30").env_clear()
            .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
            .spawn().unwrap())
    }
    fn witness(&self) -> HelperProcessWitness {
        HelperProcessWitness::pending(pidfd(self.0.id()).unwrap(), self.0.id(),
            unsafe { libc::geteuid() }, super::super::readiness_tests::binding())
    }
    fn exit_and_reap(&mut self) {
        self.0.kill().unwrap(); self.0.wait().unwrap();
    }
}
impl Drop for LiveChild {
    fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); }
}
fn actual_pid(fd: &OwnedFd) -> u32 { super::super::super::network_phase::live_pid(fd).unwrap() }

#[test]
fn pending_export_duplicates_the_original_live_child_without_activating_custody() {
    let child = LiveChild::spawn(); let witness = child.witness();
    let original_raw = witness.lifetime.as_raw_fd();
    assert!(witness.check_current().is_err());
    assert_eq!(witness.state.load(Ordering::Acquire), PENDING);
    let duplicate = witness.duplicate_live_pidfd_for_seed_enrollment().unwrap();
    assert_ne!(duplicate.as_raw_fd(), original_raw);
    assert_eq!(actual_pid(&duplicate), child.0.id());
    assert_eq!(actual_pid(&witness.lifetime), child.0.id());
    assert_ne!(unsafe { libc::fcntl(duplicate.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC, 0);
    assert_eq!(witness.state.load(Ordering::Acquire), PENDING);
    assert!(witness.check_current().is_err());
    witness.transferred().unwrap();
    let after_transfer = witness.duplicate_live_pidfd_for_seed_enrollment().unwrap();
    assert_eq!(actual_pid(&after_transfer), child.0.id());
    assert_eq!(actual_pid(&duplicate), child.0.id());
    assert_eq!(witness.lifetime.as_raw_fd(), original_raw);
    witness.check_current().unwrap();
}

#[test]
fn explicit_revocation_refuses_export_absorbingly_while_original_child_is_live() {
    let child = LiveChild::spawn(); let witness = child.witness();
    let already_duplicated = witness.duplicate_live_pidfd_for_seed_enrollment().unwrap();
    witness.revoke();
    assert!(witness.duplicate_live_pidfd_for_seed_enrollment().is_err());
    witness.control_finished(&Ok(()));
    assert!(witness.transferred().is_err());
    assert!(witness.duplicate_live_pidfd_for_seed_enrollment().is_err());
    assert_eq!(witness.state.load(Ordering::Acquire), REVOKED);
    // A previously exported fd still identifies the live original child. It is
    // DATA and cannot undo revocation or release a trusted caller's use barrier.
    assert_eq!(actual_pid(&already_duplicated), child.0.id());
}

#[test]
fn dead_original_duplicate_cannot_switch_to_a_later_live_child_or_saved_pid() {
    let mut original = LiveChild::spawn(); let mut witness = original.witness();
    let duplicate = witness.duplicate_live_pidfd_for_seed_enrollment().unwrap();
    original.exit_and_reap();
    let later = LiveChild::spawn(); let later_witness = later.witness();
    let later_duplicate = later_witness.duplicate_live_pidfd_for_seed_enrollment().unwrap();
    assert!(exited(&duplicate).unwrap()); assert!(exited(&witness.lifetime).unwrap());
    assert_eq!(actual_pid(&later_duplicate), later.0.id());
    // Simulate stale serialized/numeric state now naming a different child.
    // The export must inspect/duplicate the original held fd, never reopen this.
    witness.pid = later.0.id();
    assert!(witness.duplicate_live_pidfd_for_seed_enrollment().is_err());
    assert!(witness.transferred().is_err());
    assert!(witness.duplicate_live_pidfd_for_seed_enrollment().is_err());
    assert_eq!(witness.state.load(Ordering::Acquire), REVOKED);
    assert!(exited(&duplicate).unwrap());
    assert_eq!(actual_pid(&later_duplicate), later.0.id());
}

#[test]
fn mismatched_original_identity_is_absorbing_even_if_saved_pid_is_later_corrected() {
    let child = LiveChild::spawn(); let mut witness = child.witness();
    witness.pid = std::process::id();
    assert!(witness.duplicate_live_pidfd_for_seed_enrollment().is_err());
    witness.pid = child.0.id();
    assert!(witness.duplicate_live_pidfd_for_seed_enrollment().is_err());
    assert_eq!(witness.state.load(Ordering::Acquire), REVOKED);
    assert_eq!(actual_pid(&witness.lifetime), child.0.id());
}

#[test]
fn expired_pending_custody_cannot_export_or_revive_after_deadline_is_replaced() {
    let child = LiveChild::spawn(); let mut witness = child.witness();
    witness.deadline = Instant::now() - Duration::from_millis(1);
    assert!(witness.duplicate_live_pidfd_for_seed_enrollment().is_err());
    witness.deadline = Instant::now() + Duration::from_secs(10);
    assert!(witness.duplicate_live_pidfd_for_seed_enrollment().is_err());
    assert!(witness.transferred().is_err());
    assert_eq!(witness.state.load(Ordering::Acquire), REVOKED);
    assert_eq!(actual_pid(&witness.lifetime), child.0.id());
}
