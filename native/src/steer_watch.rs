//! Wakes the App loop the moment a request lands in the steer inbox
//! (`<repo>/.rudder/steer`), so phone and web actions apply within a frame
//! instead of on a once-a-second poll.
//!
//! Every sender writes `<name>.json.<pid>.tmp` and renames it to `<name>.json`
//! (cloud/worker/steer-inbox.mjs, src/board/daemon.ts, src/cloud.ts). The watch
//! thread only sends `LoopSignal::SteerInbox`; the App loop reads and consumes
//! the inbox itself (`App::poll_steer_inbox`). Linux uses inotify, macOS kqueue.

use std::ffi::CString;
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::thread;

use crate::LoopSignal;

/// Arm the watch on `repo`'s inbox (creating it), then wake `signals` each
/// time a request may have arrived. Returns once armed, so any request written
/// after this call produces a wake. If the inbox is deleted or moved, the watch
/// recreates it and re-arms; a failure there goes to `notices` and ends the
/// watch.
pub(crate) fn spawn(
    repo: &Path,
    signals: Sender<LoopSignal>,
    notices: Sender<String>,
) -> io::Result<()> {
    let inbox = repo.join(".rudder").join("steer");
    let mut watch = Watch::new(inbox.clone())
        .map_err(|error| io::Error::new(error.kind(), format!("watch {}: {error}", inbox.display())))?;
    thread::Builder::new()
        .name("rudder-steer-watch".to_string())
        .spawn(move || loop {
            if let Err(error) = watch.wait() {
                let _ = notices.send(format!(
                    "phone and web actions stopped reaching this dashboard: watching {} failed: {error}",
                    inbox.display()
                ));
                return;
            }
            // A send error means the loop is gone.
            if signals.send(LoopSignal::SteerInbox).is_err() {
                return;
            }
        })?;
    Ok(())
}

/// Create `.rudder/steer` under an existing repo. The repo itself is never
/// recreated, so a deleted project stays deleted.
fn ensure_inbox(inbox: &Path) -> io::Result<()> {
    let state = inbox.parent().expect("the inbox is <repo>/.rudder/steer");
    for dir in [state, inbox] {
        match fs::create_dir(dir) {
            Err(error) if error.kind() != io::ErrorKind::AlreadyExists => return Err(error),
            _ => {}
        }
    }
    Ok(())
}

fn c_path(path: &Path) -> io::Result<CString> {
    Ok(CString::new(path.as_os_str().as_bytes())?)
}

#[cfg(target_os = "linux")]
struct Watch {
    inbox: PathBuf,
    fd: OwnedFd,
    wd: libc::c_int,
}

#[cfg(target_os = "linux")]
impl Watch {
    fn new(inbox: PathBuf) -> io::Result<Self> {
        ensure_inbox(&inbox)?;
        // SAFETY: a plain syscall; the descriptor is owned right after.
        let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` is a fresh descriptor nothing else owns.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let wd = Self::add(&fd, &inbox)?;
        Ok(Self { inbox, fd, wd })
    }

    fn add(fd: &OwnedFd, inbox: &Path) -> io::Result<libc::c_int> {
        let path = c_path(inbox)?;
        // A rename into the inbox is how every sender delivers; a closed write
        // covers one written in place. IN_IGNORED (watch gone) always arrives.
        let mask = libc::IN_MOVED_TO | libc::IN_CLOSE_WRITE | libc::IN_MOVE_SELF;
        // SAFETY: `path` is a valid NUL-terminated string for the call.
        let wd = unsafe { libc::inotify_add_watch(fd.as_raw_fd(), path.as_ptr(), mask) };
        if wd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(wd)
    }

    /// Block until the inbox may hold a new request.
    fn wait(&mut self) -> io::Result<()> {
        const HEADER: usize = std::mem::size_of::<libc::inotify_event>();
        let mut buf = [0u8; 8192];
        loop {
            // SAFETY: reads into a buffer of the given length.
            let read = unsafe { libc::read(self.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            if read < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            let read = read as usize;
            let (mut request, mut lost) = (false, false);
            let mut offset = 0;
            while offset + HEADER <= read {
                // SAFETY: the kernel wrote a whole event header at `offset`.
                let event: libc::inotify_event =
                    unsafe { std::ptr::read_unaligned(buf.as_ptr().add(offset).cast()) };
                let name = &buf[offset + HEADER..offset + HEADER + event.len as usize];
                offset += HEADER + event.len as usize;
                if event.mask & libc::IN_Q_OVERFLOW != 0 {
                    request = true;
                } else if event.wd != self.wd {
                    // Left over from a watch replaced below.
                } else if event.mask & libc::IN_MOVE_SELF != 0 {
                    // The watch would follow the moved directory. Drop it (the
                    // kernel may already have) and watch the inbox path again.
                    // SAFETY: a plain syscall on our own descriptor.
                    unsafe { libc::inotify_rm_watch(self.fd.as_raw_fd(), self.wd) };
                    lost = true;
                } else if event.mask & libc::IN_IGNORED != 0 {
                    lost = true;
                } else if name.split(|byte| *byte == 0).next().is_some_and(|name| name.ends_with(b".json")) {
                    request = true;
                }
            }
            if lost {
                ensure_inbox(&self.inbox)?;
                self.wd = Self::add(&self.fd, &self.inbox)?;
                // Anything written while unwatched is picked up by this wake.
                request = true;
            }
            if request {
                return Ok(());
            }
        }
    }
}

#[cfg(target_os = "macos")]
struct Watch {
    inbox: PathBuf,
    queue: OwnedFd,
    dir: OwnedFd,
}

#[cfg(target_os = "macos")]
impl Watch {
    fn new(inbox: PathBuf) -> io::Result<Self> {
        ensure_inbox(&inbox)?;
        // SAFETY: a plain syscall; the descriptor is owned right after.
        let queue = unsafe { libc::kqueue() };
        if queue < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `queue` is a fresh descriptor nothing else owns.
        let queue = unsafe { OwnedFd::from_raw_fd(queue) };
        let dir = Self::register(&queue, &inbox)?;
        Ok(Self { inbox, queue, dir })
    }

    fn register(queue: &OwnedFd, inbox: &Path) -> io::Result<OwnedFd> {
        let path = c_path(inbox)?;
        // SAFETY: `path` is a valid NUL-terminated string for the call.
        let dir = unsafe { libc::open(path.as_ptr(), libc::O_EVTONLY | libc::O_CLOEXEC) };
        if dir < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `dir` is a fresh descriptor nothing else owns.
        let dir = unsafe { OwnedFd::from_raw_fd(dir) };
        // NOTE_WRITE on a directory: an entry was added, removed or renamed.
        // kqueue gives no names, so the temp file and our own consumption
        // wake too; the App loop's read of an empty inbox is cheap.
        let change = libc::kevent {
            ident: dir.as_raw_fd() as libc::uintptr_t,
            filter: libc::EVFILT_VNODE,
            flags: libc::EV_ADD | libc::EV_CLEAR,
            fflags: libc::NOTE_WRITE | libc::NOTE_DELETE | libc::NOTE_RENAME | libc::NOTE_REVOKE,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        // SAFETY: one valid change, no event output.
        let result = unsafe {
            libc::kevent(queue.as_raw_fd(), &change, 1, std::ptr::null_mut(), 0, std::ptr::null())
        };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(dir)
    }

    /// Block until the inbox may hold a new request.
    fn wait(&mut self) -> io::Result<()> {
        loop {
            // SAFETY: an all-zero kevent is a valid output slot.
            let mut event: libc::kevent = unsafe { std::mem::zeroed() };
            // SAFETY: no changes, one output slot, no timeout.
            let count = unsafe {
                libc::kevent(self.queue.as_raw_fd(), std::ptr::null(), 0, &mut event, 1, std::ptr::null())
            };
            if count < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if count == 0 {
                continue;
            }
            if event.fflags & (libc::NOTE_DELETE | libc::NOTE_RENAME | libc::NOTE_REVOKE) != 0 {
                // The directory went away: watch the inbox path again. Closing
                // the old descriptor (on assignment) drops its registration.
                ensure_inbox(&self.inbox)?;
                self.dir = Self::register(&self.queue, &self.inbox)?;
            }
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::{self, Receiver};
    use std::time::{Duration, Instant};

    struct TempDir(PathBuf);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn deliver(inbox: &Path, name: &str) {
        let file = inbox.join(name);
        let temp = inbox.join(format!("{name}.1.tmp"));
        fs::write(&temp, "{}").unwrap();
        fs::rename(&temp, &file).unwrap();
    }

    fn woke(wakes: &Receiver<LoopSignal>, within: Duration) -> bool {
        matches!(wakes.recv_timeout(within), Ok(LoopSignal::SteerInbox))
    }

    #[test]
    fn a_request_wakes_the_loop_even_after_the_inbox_is_deleted() {
        let repo = TempDir(std::env::temp_dir().join(format!("rudder-steer-watch-{}", std::process::id())));
        let _ = fs::remove_dir_all(&repo.0);
        fs::create_dir_all(&repo.0).unwrap();
        let inbox = repo.0.join(".rudder").join("steer");
        let (signals, wakes) = mpsc::channel();
        let (notices, notice_rx) = mpsc::channel();
        spawn(&repo.0, signals, notices).unwrap();

        deliver(&inbox, "0001-request.json");
        assert!(woke(&wakes, Duration::from_secs(5)), "a renamed-in request wakes the loop");

        // Deleted under the dashboard: the watch recreates the inbox and keeps
        // waking for new requests.
        fs::remove_dir_all(repo.0.join(".rudder")).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !inbox.is_dir() {
            assert!(Instant::now() < deadline, "the inbox comes back");
            std::thread::sleep(Duration::from_millis(5));
        }
        while woke(&wakes, Duration::from_millis(200)) {}
        deliver(&inbox, "0002-request.json");
        assert!(woke(&wakes, Duration::from_secs(5)), "the re-armed watch wakes the loop");
        assert!(notice_rx.try_recv().is_err(), "no watch failure");
    }
}
