//! Observation mechanism only. No positive Root ACK, grant, registration or
//! published owner is fabricated. Production passes only this secondary slot.
use super::*;
use super::super::super::{StartupObservationDelivery, StartupObservationOwner, OBSERVATION_LIMIT};
use std::cell::Cell;

const LINE: &[u8] = b"secondary qualification observation DATA only\n";

fn pipe() -> (OwnedFd, OwnedFd) {
    let mut raw = [-1; 2];
    assert_eq!(unsafe { libc::pipe2(raw.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) }, 0);
    (unsafe { OwnedFd::from_raw_fd(raw[0]) }, unsafe { OwnedFd::from_raw_fd(raw[1]) })
}

fn ignored_sigpipe() {
    // Query actual disposition; never change process-global signal handling.
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    assert_eq!(unsafe { libc::sigaction(libc::SIGPIPE, std::ptr::null(), &mut action) }, 0);
    assert_eq!(action.sa_sigaction, libc::SIG_IGN, "real pipe cases require actual safe Rust SIGPIPE disposition");
}

fn flags(fd: &OwnedFd) -> libc::c_int {
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    assert!(flags >= 0);
    flags
}

fn assert_retained(observation: &StartupObservationOwner, original: &OwnedFd, expected_flags: libc::c_int) {
    assert!(observation.attempted);
    assert_eq!(observation.bytes.as_deref(), Some(LINE));
    let output = observation.output.as_ref().expect("actual output duplicate retained");
    assert_ne!(output.as_raw_fd(), original.as_raw_fd());
    assert_ne!(unsafe { libc::fcntl(output.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC, 0);
    assert_eq!(flags(original), expected_flags);
    assert_eq!(flags(output), expected_flags);
    let mut original_stat: libc::stat = unsafe { std::mem::zeroed() };
    let mut output_stat: libc::stat = unsafe { std::mem::zeroed() };
    assert_eq!(unsafe { libc::fstat(original.as_raw_fd(), &mut original_stat) }, 0);
    assert_eq!(unsafe { libc::fstat(output.as_raw_fd(), &mut output_stat) }, 0);
    assert_eq!((original_stat.st_dev, original_stat.st_ino), (output_stat.st_dev, output_stat.st_ino));
    assert!(observation.error.is_none() && observation.panic.is_none());
}

#[test]
fn qualification_observation_real_full_blocking_pipe_is_one_attempt_and_restores_flags() {
    ignored_sigpipe();
    let (read, write) = pipe();
    let chunk = [b'x'; 4096];
    let mut full = false;
    for _ in 0..=256 {
        let n = unsafe { libc::write(write.as_raw_fd(), chunk.as_ptr().cast(), chunk.len()) };
        if n < 0 {
            assert_eq!(io::Error::last_os_error().kind(), io::ErrorKind::WouldBlock);
            full = true;
            break;
        }
        assert_eq!(n, chunk.len() as isize);
    }
    assert!(full, "real bounded pipe reached backpressure");
    let original_flags = flags(&write) & !libc::O_NONBLOCK;
    assert_eq!(unsafe { libc::fcntl(write.as_raw_fd(), libc::F_SETFL, original_flags) }, 0);
    let mut observation = StartupObservationOwner::new();
    observe_qualification_once(&mut observation, write.as_raw_fd(), || Ok(LINE.to_vec()));
    assert_eq!(observation.delivery, StartupObservationDelivery::Blocked);
    assert_retained(&observation, &write, original_flags);
    let held = observation.output.as_ref().unwrap().as_raw_fd();
    let replayed = Cell::new(false);
    observe_qualification_once(&mut observation, -1, || { replayed.set(true); Ok(b"replacement".to_vec()) });
    assert!(!replayed.get());
    assert_eq!(observation.output.as_ref().unwrap().as_raw_fd(), held);
    assert_eq!(observation.delivery, StartupObservationDelivery::Blocked);
    assert_eq!(observation.bytes.as_deref(), Some(LINE));
    drop(read);
}

#[test]
fn qualification_observation_real_closed_reader_is_retained_failure_without_sigpipe() {
    ignored_sigpipe();
    let (read, write) = pipe();
    drop(read);
    let original_flags = flags(&write) & !libc::O_NONBLOCK;
    assert_eq!(unsafe { libc::fcntl(write.as_raw_fd(), libc::F_SETFL, original_flags) }, 0);
    let mut observation = StartupObservationOwner::new();
    observe_qualification_once(&mut observation, write.as_raw_fd(), || Ok(LINE.to_vec()));
    assert_eq!(observation.delivery, StartupObservationDelivery::Failed);
    assert_retained(&observation, &write, original_flags);
    observe_qualification_once(&mut observation, -1, || panic!("closed observer must not retry"));
    assert_eq!(observation.delivery, StartupObservationDelivery::Failed);
}

#[test]
fn qualification_observation_actual_delivered_bytes_are_exact_and_not_authority() {
    ignored_sigpipe();
    let (read, write) = pipe();
    let original_flags = flags(&write);
    let mut observation = StartupObservationOwner::new();
    observe_qualification_once(&mut observation, write.as_raw_fd(), || Ok(LINE.to_vec()));
    assert_eq!(observation.delivery, StartupObservationDelivery::Delivered);
    assert_retained(&observation, &write, original_flags);
    let mut actual = [0u8; 128];
    let n = unsafe { libc::read(read.as_raw_fd(), actual.as_mut_ptr().cast(), actual.len()) };
    assert_eq!(n, LINE.len() as isize);
    assert_eq!(&actual[..n as usize], LINE);
}

#[test]
fn qualification_observation_invalid_output_retains_bytes_and_original_error_once() {
    let mut observation = StartupObservationOwner::new();
    observe_qualification_once(&mut observation, -1, || Ok(LINE.to_vec()));
    assert_eq!(observation.delivery, StartupObservationDelivery::Unavailable);
    assert_eq!(observation.bytes.as_deref(), Some(LINE));
    assert!(observation.output.is_none() && observation.panic.is_none());
    let message = observation.error.as_ref().unwrap().to_string();
    assert!(message.contains("original stderr unavailable"));
    observe_qualification_once(&mut observation, libc::STDERR_FILENO, || panic!("failed observer must not retry"));
    assert_eq!(observation.error.as_ref().unwrap().to_string(), message);
}

#[test]
fn qualification_observation_oversized_bytes_are_retained_before_any_output_allocation() {
    let mut observation = StartupObservationOwner::new();
    observe_qualification_once(&mut observation, -1, || Ok(vec![b'x'; OBSERVATION_LIMIT + 1]));
    assert_eq!(observation.delivery, StartupObservationDelivery::Unavailable);
    assert_eq!(observation.bytes.as_ref().unwrap().len(), OBSERVATION_LIMIT + 1);
    assert!(observation.output.is_none() && observation.error.is_some() && observation.panic.is_none());
}

#[test]
fn qualification_observation_preparation_error_and_panic_are_absorbed_in_separate_carrier() {
    // A staged refusal carrier only; never set published or manufacture an ACK.
    let fd: OwnedFd = File::open("/dev/null").unwrap().into();
    let mut qualification = RootQualification::staged(fd);
    let before = (qualification.published, qualification.closed, qualification.closed(), qualification.failure.is_some());
    observe_qualification_once(&mut qualification.observation, -1, || anyhow::bail!("actual observer preparation failure"));
    assert_eq!(qualification.observation.delivery, StartupObservationDelivery::Unavailable);
    assert!(qualification.observation.error.as_ref().unwrap().to_string().contains("actual observer preparation failure"));
    assert_eq!((qualification.published, qualification.closed, qualification.closed(), qualification.failure.is_some()), before);

    let fd: OwnedFd = File::open("/dev/null").unwrap().into();
    let mut qualification = RootQualification::staged(fd);
    let before = (qualification.published, qualification.closed, qualification.closed(), qualification.failure.is_some());
    // resume_unwind exercises actual catch_unwind without a test panic hook
    // itself writing to global stderr. It issues no success/Root authority.
    observe_qualification_once(&mut qualification.observation, -1, || std::panic::resume_unwind(Box::new("actual observer unwind")));
    assert_eq!(qualification.observation.delivery, StartupObservationDelivery::Uncertain);
    assert!(qualification.observation.panic.as_ref().unwrap().is::<&'static str>());
    assert!(qualification.observation.error.is_none() && qualification.observation.output.is_none());
    assert_eq!((qualification.published, qualification.closed, qualification.closed(), qualification.failure.is_some()), before);
    observe_qualification_once(&mut qualification.observation, -1, || panic!("unwound observer must not retry"));
}
