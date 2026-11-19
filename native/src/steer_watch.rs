//! Wakes the App loop the moment a request lands in the steer inbox
//! (`<repo>/.rudder/steer`), so phone and web actions apply within a frame
//! instead of on a once-a-second poll.
//!
//! Every sender creates the inbox if needed (`mkdir -p`), writes
//! `<name>.json.<pid>.tmp` and renames it to `<name>.json`
//! (cloud/worker/steer-inbox.mjs, src/board/daemon.ts, src/cloud.ts). The watch
//! thread only sends `LoopSignal::SteerInbox`; the App loop reads and consumes
//! the inbox itself (`App::poll_steer_inbox`). Linux uses inotify, macOS kqueue.
//!
//! The watch creates the inbox once, at startup, and never again: recreating it
//! while `rm -rf` walks the repo makes that removal fail "directory not empty".
//! While the inbox is missing, the watch sits on its nearest existing ancestor,
//! up to the repo root, until a sender recreates it. A deleted repo ends the
//! watch quietly: a deleted project stays deleted.

use std::ffi::CString;
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::thread;

use crate::LoopSignal;

/// Create `repo`'s inbox, arm the watch on it, then wake `signals` each time a
/// request may have arrived. Returns once armed, so any request written after
/// this call produces a wake, also after the inbox was deleted and a sender
/// recreated it. A watch failure goes to `notices` and ends the watch.
pub(crate) fn spawn(
    repo: &Path,
    signals: Sender<LoopSignal>,
    notices: Sender<String>,
) -> io::Result<()> {
    let inbox = repo.join(".rudder").join("steer");
    let context = |error: io::Error| io::Error::new(error.kind(), format!("watch {}: {error}", inbox.display()));
    create_inbox(&inbox).map_err(context)?;
    let mut watch = Watch::new(repo).map_err(context)?;
    thread::Builder::new()
        .name("rudder-steer-watch".to_string())
        .spawn(move || loop {
            match watch.wait() {
                Ok(true) => {}
                // The repo is gone: nothing will write here again.
                Ok(false) => return,
                Err(error) => {
                    let _ = notices.send(format!(
                        "phone and web actions stopped reaching this dashboard: watching {} failed: {error}",
                        inbox.display()
                    ));
                    return;
                }
            }
            // A send error means the loop is gone.
            if signals.send(LoopSignal::SteerInbox).is_err() {
                return;
            }
        })?;
    Ok(())
}

/// Create `.rudder/steer` under an existing repo, at startup only.
fn create_inbox(inbox: &Path) -> io::Result<()> {
    let state = inbox.parent().expect("the inbox is <repo>/.rudder/steer");
    for dir in [state, inbox] {
        match fs::create_dir(dir) {
            Err(error) if error.kind() != io::ErrorKind::AlreadyExists => return Err(error),
            _ => {}
        }
    }
    Ok(())
}

/// What the watch can sit on, nearest first: the inbox, `.rudder`, the repo.
fn levels(repo: &Path) -> [PathBuf; 3] {
    let state = repo.join(".rudder");
    [state.join("steer"), state, repo.to_path_buf()]
}

/// Nothing at all is at `path` now. A dangling symlink is something, so a
/// broken link fails the watch instead of looping.
fn is_gone(path: &Path) -> bool {
    matches!(fs::symlink_metadata(path), Err(error) if error.kind() == io::ErrorKind::NotFound)
}

/// Arm a watch on the inbox (level 0) or, while it is missing, on its nearest
/// existing ancestor (level 1 or 2) for the next level down to appear.
/// `add(path, level)` arms one path; `remove` drops a watch whose child
/// appeared before it was armed, which no event would report. None: the repo
/// itself is gone. Creates nothing.
fn arm<W>(
    levels: &[PathBuf; 3],
    mut add: impl FnMut(&Path, usize) -> io::Result<W>,
    mut remove: impl FnMut(W),
) -> io::Result<Option<(usize, W)>> {
    'nearest: loop {
        for (level, path) in levels.iter().enumerate() {
            match add(path, level) {
                Ok(watch) if level > 0 && !is_gone(&levels[level - 1]) => {
                    remove(watch);
                    continue 'nearest;
                }
                Ok(watch) => return Ok(Some((level, watch))),
                Err(error) if error.kind() == io::ErrorKind::NotFound && is_gone(path) => {}
                Err(error) => return Err(error),
            }
        }
        return Ok(None);
    }
}

fn c_path(path: &Path) -> io::Result<CString> {
    Ok(CString::new(path.as_os_str().as_bytes())?)
}

#[cfg(target_os = "linux")]
struct Watch {
    levels: [PathBuf; 3],
    fd: OwnedFd,
    wd: libc::c_int,
    level: usize,
}

#[cfg(target_os = "linux")]
impl Watch {
    fn new(repo: &Path) -> io::Result<Self> {
        // SAFETY: a plain syscall; the descriptor is owned right after.
        let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` is a fresh descriptor nothing else owns.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let levels = levels(repo);
        let (level, wd) = arm(&levels, |path, level| Self::add(&fd, path, level), |wd| Self::remove(&fd, wd))?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "the repo is gone"))?;
        Ok(Self { levels, fd, wd, level })
    }

    fn add(fd: &OwnedFd, path: &Path, level: usize) -> io::Result<libc::c_int> {
        let path = c_path(path)?;
        // On the inbox, a rename into it is how every sender delivers; a closed
        // write covers one written in place. On an ancestor, the next level
        // down appearing. IN_IGNORED (watch gone) always arrives.
        let events = if level == 0 {
            libc::IN_MOVED_TO | libc::IN_CLOSE_WRITE
        } else {
            libc::IN_CREATE | libc::IN_MOVED_TO
        };
        let mask = events | libc::IN_MOVE_SELF | libc::IN_ONLYDIR;
        // SAFETY: `path` is a valid NUL-terminated string for the call.
        let wd = unsafe { libc::inotify_add_watch(fd.as_raw_fd(), path.as_ptr(), mask) };
        if wd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(wd)
    }

    fn remove(fd: &OwnedFd, wd: libc::c_int) {
        // SAFETY: a plain syscall on our own descriptor. A watch the kernel
        // already dropped fails harmlessly.
        unsafe { libc::inotify_rm_watch(fd.as_raw_fd(), wd) };
    }

    /// Block until the inbox may hold a new request. False: the repo is gone.
    fn wait(&mut self) -> io::Result<bool> {
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
            let child = (self.level > 0).then(|| self.levels[self.level - 1].file_name()).flatten();
            let (mut request, mut moved) = (false, false);
            let mut offset = 0;
            while offset + HEADER <= read {
                // SAFETY: the kernel wrote a whole event header at `offset`.
                let event: libc::inotify_event =
                    unsafe { std::ptr::read_unaligned(buf.as_ptr().add(offset).cast()) };
                let name = &buf[offset + HEADER..offset + HEADER + event.len as usize];
                let name = name.split(|byte| *byte == 0).next().unwrap_or_default();
                offset += HEADER + event.len as usize;
                if event.mask & libc::IN_Q_OVERFLOW != 0 {
                    // Events were lost, a self event among them maybe.
                    moved = true;
                } else if event.wd != self.wd {
                    // Left over from a watch replaced below.
                } else if event.mask & (libc::IN_MOVE_SELF | libc::IN_IGNORED) != 0 {
                    // Gone, or moved (the watch would follow the directory).
                    moved = true;
                } else if let Some(child) = child {
                    moved |= name == child.as_bytes();
                } else if name.ends_with(b".json") {
                    request = true;
                }
            }
            if moved {
                let (fd, levels) = (&self.fd, &self.levels);
                Self::remove(fd, self.wd);
                let Some((level, wd)) = arm(levels, |path, level| Self::add(fd, path, level), |wd| Self::remove(fd, wd))?
                else {
                    return Ok(false);
                };
                (self.level, self.wd) = (level, wd);
                // Anything written while unwatched is picked up by this wake.
                request |= level == 0;
            }
            if request {
                return Ok(true);
            }
        }
    }
}

#[cfg(target_os = "macos")]
struct Watch {
    levels: [PathBuf; 3],
    queue: OwnedFd,
    dir: OwnedFd,
    level: usize,
}

#[cfg(target_os = "macos")]
impl Watch {
    fn new(repo: &Path) -> io::Result<Self> {
        // SAFETY: a plain syscall; the descriptor is owned right after.
        let queue = unsafe { libc::kqueue() };
        if queue < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `queue` is a fresh descriptor nothing else owns.
        let queue = unsafe { OwnedFd::from_raw_fd(queue) };
        let levels = levels(repo);
        let (level, dir) = arm(&levels, |path, _| Self::register(&queue, path), drop)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "the repo is gone"))?;
        Ok(Self { levels, queue, dir, level })
    }

    fn register(queue: &OwnedFd, path: &Path) -> io::Result<OwnedFd> {
        let path = c_path(path)?;
        // SAFETY: `path` is a valid NUL-terminated string for the call.
        let dir = unsafe { libc::open(path.as_ptr(), libc::O_EVTONLY | libc::O_DIRECTORY | libc::O_CLOEXEC) };
        if dir < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `dir` is a fresh descriptor nothing else owns.
        let dir = unsafe { OwnedFd::from_raw_fd(dir) };
        // NOTE_WRITE on a directory: an entry was added, removed or renamed.
        // kqueue gives no names, so on the inbox the temp file and our own
        // consumption wake too (the App loop's read of an empty inbox is
        // cheap), and on an ancestor any entry change looks again.
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

    /// Block until the inbox may hold a new request. False: the repo is gone.
    fn wait(&mut self) -> io::Result<bool> {
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
            let gone = event.fflags & (libc::NOTE_DELETE | libc::NOTE_RENAME | libc::NOTE_REVOKE) != 0;
            if !gone && self.level == 0 {
                return Ok(true);
            }
            // The directory went away, or an ancestor's entries changed: look
            // again. Closing the old descriptor (on assignment) drops its
            // registration.
            let queue = &self.queue;
            let Some((level, dir)) = arm(&self.levels, |path, _| Self::register(queue, path), drop)? else {
                return Ok(false);
            };
            (self.level, self.dir) = (level, dir);
            // Anything written while unwatched is picked up by this wake.
            if level == 0 {
                return Ok(true);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::{self, Receiver};
    use std::time::Duration;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("rudder-steer-watch-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// What every sender does: create the inbox, write a temp file, rename it.
    fn deliver(inbox: &Path, name: &str) {
        fs::create_dir_all(inbox).unwrap();
        let file = inbox.join(name);
        let temp = inbox.join(format!("{name}.1.tmp"));
        fs::write(&temp, "{}").unwrap();
        fs::rename(&temp, &file).unwrap();
    }

    fn woke(wakes: &Receiver<LoopSignal>, within: Duration) -> bool {
        matches!(wakes.recv_timeout(within), Ok(LoopSignal::SteerInbox))
    }

    #[test]
    fn a_request_wakes_the_loop_after_a_sender_recreates_the_deleted_inbox() {
        let repo = TempDir::new("recreate");
        let state = repo.0.join(".rudder");
        let inbox = state.join("steer");
        let (signals, wakes) = mpsc::channel();
        let (notices, notice_rx) = mpsc::channel();
        spawn(&repo.0, signals, notices).unwrap();
        assert!(inbox.is_dir(), "startup creates the inbox");

        deliver(&inbox, "0001-request.json");
        assert!(woke(&wakes, Duration::from_secs(5)), "a renamed-in request wakes the loop");

        for round in 0..3 {
            // Deleted under the dashboard: the removal succeeds and nothing
            // comes back on its own.
            fs::remove_dir_all(&state).unwrap();
            std::thread::sleep(Duration::from_millis(100));
            assert!(is_gone(&state), "round {round}: the watch never recreates the inbox");
            while woke(&wakes, Duration::from_millis(100)) {}

            // A sender recreates it and its request still wakes the loop.
            deliver(&inbox, &format!("{round}-recreated.json"));
            assert!(
                woke(&wakes, Duration::from_secs(5)),
                "round {round}: the request in the recreated inbox wakes the loop: {:?}",
                notice_rx.try_recv()
            );
            while woke(&wakes, Duration::from_millis(100)) {}
            deliver(&inbox, &format!("{round}-again.json"));
            assert!(woke(&wakes, Duration::from_secs(5)), "round {round}: the recreated inbox stays watched");
        }

        // With the loop gone, the next wake ends the watch, which drops its
        // notice sender without a notice.
        drop(wakes);
        deliver(&inbox, "0003-request.json");
        assert!(
            matches!(notice_rx.recv_timeout(Duration::from_secs(5)), Err(mpsc::RecvTimeoutError::Disconnected)),
            "the watch ends with the loop and reports no failure"
        );
    }

    #[test]
    fn deleting_the_repo_under_the_watch_succeeds_and_ends_it_quietly() {
        let parent = TempDir::new("delete");
        let repo = parent.0.join("repo");
        fs::create_dir_all(repo.join("src")).unwrap();
        fs::write(repo.join("src").join("main.rs"), "fn main() {}").unwrap();
        let (signals, wakes) = mpsc::channel();
        let (notices, notice_rx) = mpsc::channel();
        spawn(&repo, signals, notices).unwrap();
        deliver(&repo.join(".rudder").join("steer"), "0001-request.json");
        assert!(woke(&wakes, Duration::from_secs(5)), "a renamed-in request wakes the loop");

        // `rm -rf <repo>` with the dashboard still running: no "directory not
        // empty", and the watch ends without a notice.
        fs::remove_dir_all(&repo).unwrap();
        assert!(
            matches!(notice_rx.recv_timeout(Duration::from_secs(5)), Err(mpsc::RecvTimeoutError::Disconnected)),
            "the watch ends quietly once the repo is gone"
        );
        assert!(is_gone(&repo), "a deleted project stays deleted");
        drop(wakes);
    }
}
