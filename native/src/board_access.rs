//! Host-only browser callback. Tickets and browser URLs remain in the Node owner.
//! The App loop starts/polls one request; this worker never mutates App state.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Target {
    Project,
    Usage,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OpenResult {
    Opened,
    Unavailable,
}

static OPENING: AtomicBool = AtomicBool::new(false);

/// At most one bounded callback is in flight, including repeated hotkeys.
pub(crate) fn begin_open(repo: PathBuf, target: Target) -> Receiver<OpenResult> {
    let (sender, receiver) = mpsc::sync_channel(1);
    if OPENING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        let _ = sender.try_send(OpenResult::Unavailable);
        return receiver;
    }
    struct Release;
    impl Drop for Release {
        fn drop(&mut self) {
            OPENING.store(false, Ordering::Release);
        }
    }
    let release = Release;
    let failed = sender.clone();
    if std::thread::Builder::new()
        .name("board-browser".into())
        .spawn(move || {
            let _release = release;
            let opened = host_home()
                .and_then(|home| open_at(&repo, &home, target).ok())
                .is_some();
            let _ = sender.try_send(if opened {
                OpenResult::Opened
            } else {
                OpenResult::Unavailable
            });
        })
        .is_err()
    {
        let _ = failed.try_send(OpenResult::Unavailable);
    }
    receiver
}

fn host_home() -> Option<PathBuf> {
    let home = std::env::var("RUDDER_HOME")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(|value| PathBuf::from(value.trim()))
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(|value| PathBuf::from(value).join(".rudder"))
        })?;
    let home = if home.is_absolute() {
        home
    } else {
        std::env::current_dir().ok()?.join(home)
    };
    // Match Node path.resolve before choosing the short-socket fallback.
    let mut resolved = PathBuf::new();
    for component in home.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                resolved.pop();
            }
            component => resolved.push(component.as_os_str()),
        }
    }
    Some(resolved)
}

#[cfg(not(unix))]
fn open_at(_repo: &std::path::Path, _home: &std::path::Path, _target: Target) -> Result<(), ()> {
    Err(())
}

#[cfg(unix)]
mod unix {
    use super::Target;
    use serde::Deserialize;
    use sha2::{Digest, Sha256};
    use std::fs::{self, File, OpenOptions};
    use std::io::{Read, Write};
    use std::os::fd::{FromRawFd, IntoRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
    use std::os::unix::net::UnixStream;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    const MAX_FRAME: u64 = 4096;
    const DEADLINE: Duration = Duration::from_millis(1500);

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Descriptor {
        format_version: u32,
        instance_id: String,
        origin: String,
        repo_root: String,
        process_id: u32,
        control_mode: String,
    }

    fn private_meta(meta: &fs::Metadata) -> bool {
        meta.uid() == unsafe { libc::getuid() } && meta.mode() & 0o077 == 0
    }

    pub(super) fn paths(repo: &Path, home: &Path) -> Result<(PathBuf, PathBuf, PathBuf), ()> {
        let repo = repo.canonicalize().map_err(|_| ())?;
        let key = format!("{:x}", Sha256::digest(repo.to_str().ok_or(())?.as_bytes()));
        let name = &key[..32];
        let mut directory = home.join("board");
        if directory
            .join(format!("{name}.sock"))
            .as_os_str()
            .as_bytes()
            .len()
            >= 100
        {
            directory = PathBuf::from(format!("/tmp/rudder-board-{}", unsafe { libc::getuid() }));
        }
        Ok((
            repo,
            directory.join(format!("{name}.json")),
            directory.join(format!("{name}.sock")),
        ))
    }

    fn descriptor(file: &Path) -> Result<Descriptor, ()> {
        let parent = fs::symlink_metadata(file.parent().ok_or(())?).map_err(|_| ())?;
        if !parent.is_dir() || !private_meta(&parent) {
            return Err(());
        }
        let mut input: File = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(file)
            .map_err(|_| ())?;
        let meta = input.metadata().map_err(|_| ())?;
        if !meta.is_file() || !private_meta(&meta) || meta.nlink() != 1 || meta.len() > MAX_FRAME {
            return Err(());
        }
        let mut bytes = Vec::new();
        Read::by_ref(&mut input)
            .take(MAX_FRAME + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| ())?;
        if bytes.len() as u64 > MAX_FRAME {
            return Err(());
        }
        let descriptor: Descriptor = serde_json::from_slice(&bytes).map_err(|_| ())?;
        let id = uuid::Uuid::parse_str(&descriptor.instance_id).map_err(|_| ())?;
        let suffix = descriptor
            .origin
            .strip_prefix("http://127.0.0.1")
            .ok_or(())?;
        let port = if suffix.is_empty() {
            80
        } else {
            suffix
                .strip_prefix(':')
                .ok_or(())?
                .parse::<u16>()
                .map_err(|_| ())?
        };
        let origin = if port == 80 {
            "http://127.0.0.1".to_string()
        } else {
            format!("http://127.0.0.1:{port}")
        };
        if descriptor.format_version != 1
            || id.to_string() != descriptor.instance_id
            || port == 0
            || descriptor.origin != origin
            || descriptor.process_id == 0
            || !matches!(descriptor.control_mode.as_str(), "projector" | "scheduler")
        {
            return Err(());
        }
        Ok(descriptor)
    }

    fn remaining(until: Instant) -> Result<Duration, ()> {
        until
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())
            .ok_or(())
    }

    fn connect(path: &Path, until: Instant) -> Result<UnixStream, ()> {
        // mio creates a nonblocking close-on-exec socket. Connecting cannot
        // stall the App or leak a private control FD into a concurrent child.
        let mut stream = mio::net::UnixStream::connect(path).map_err(|_| ())?;
        let mut poll = mio::Poll::new().map_err(|_| ())?;
        poll.registry()
            .register(&mut stream, mio::Token(0), mio::Interest::WRITABLE)
            .map_err(|_| ())?;
        let mut events = mio::Events::with_capacity(1);
        loop {
            match poll.poll(&mut events, Some(remaining(until)?)) {
                Ok(()) if !events.is_empty() => break,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                _ => return Err(()),
            }
        }
        if stream.take_error().map_err(|_| ())?.is_some() || stream.peer_addr().is_err() {
            return Err(());
        }
        poll.registry().deregister(&mut stream).map_err(|_| ())?;
        let stream = unsafe { UnixStream::from_raw_fd(stream.into_raw_fd()) };
        stream.set_nonblocking(false).map_err(|_| ())?;
        Ok(stream)
    }

    pub(super) fn open_at(repo: &Path, home: &Path, target: Target) -> Result<(), ()> {
        let until = Instant::now() + DEADLINE;
        let (repo, file, socket) = paths(repo, home)?;
        let descriptor = descriptor(&file)?;
        if Path::new(&descriptor.repo_root) != repo {
            return Err(());
        }
        let meta = fs::symlink_metadata(&socket).map_err(|_| ())?;
        if !meta.file_type().is_socket() || !private_meta(&meta) {
            return Err(());
        }
        let mut stream = connect(&socket, until)?;
        stream
            .set_write_timeout(Some(remaining(until)?))
            .map_err(|_| ())?;
        let request = serde_json::json!({ "format_version": 1, "instance_id": descriptor.instance_id,
            "action": "open", "target": match target { Target::Project => "project", Target::Usage => "usage" } });
        stream
            .write_all(format!("{request}\n").as_bytes())
            .map_err(|_| ())?;
        let mut reply = Vec::new();
        loop {
            stream
                .set_read_timeout(Some(remaining(until)?))
                .map_err(|_| ())?;
            let mut bytes = [0u8; 512];
            let count = stream.read(&mut bytes).map_err(|_| ())?;
            if count == 0 || reply.len() + count > MAX_FRAME as usize {
                return Err(());
            }
            reply.extend_from_slice(&bytes[..count]);
            if let Some(end) = reply.iter().position(|byte| *byte == b'\n') {
                let value: serde_json::Value =
                    serde_json::from_slice(&reply[..end]).map_err(|_| ())?;
                let object = value.as_object().ok_or(())?;
                return if object.len() == 2 && value["format_version"] == 1 && value["ok"] == true {
                    Ok(())
                } else {
                    Err(())
                };
            }
        }
    }
}

#[cfg(unix)]
use unix::open_at;

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::os::unix::net::UnixListener;
    use std::time::{Duration, Instant};

    struct Fixture {
        root: PathBuf,
        repo: PathBuf,
        home: PathBuf,
        file: PathBuf,
        socket: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            // Keep the full descriptor socket under Darwin's portable bound,
            // including /private/tmp. Test resources then stay in this root
            // rather than the production per-user short-path fallback.
            let root = PathBuf::from("/tmp").join(format!(
                "rb-{}",
                &uuid::Uuid::new_v4().simple().to_string()[..12]
            ));
            fs::create_dir(&root).unwrap();
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
            let root = root.canonicalize().unwrap();
            let repo = root.join("repo");
            let home = root.join("home");
            fs::create_dir_all(&repo).unwrap();
            let (_, file, socket) = unix::paths(&repo, &home).unwrap();
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::set_permissions(file.parent().unwrap(), fs::Permissions::from_mode(0o700)).unwrap();
            fs::write(&file, serde_json::json!({ "format_version": 1, "instance_id": "11111111-1111-4111-8111-111111111111",
                "origin": "http://127.0.0.1:4774", "repo_root": repo.canonicalize().unwrap(), "process_id": std::process::id(),
                "control_mode": "projector" }).to_string()).unwrap();
            fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
            Self {
                root,
                repo,
                home,
                file,
                socket,
            }
        }
        fn listener(&self) -> UnixListener {
            let listener = UnixListener::bind(&self.socket).unwrap();
            fs::set_permissions(&self.socket, fs::Permissions::from_mode(0o600)).unwrap();
            listener
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn private_open_sends_only_a_fixed_target_and_returns_no_capability() {
        let f = Fixture::new();
        let listener = f.listener();
        let worker = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut input = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut input)
                .unwrap();
            let value: serde_json::Value = serde_json::from_str(&input).unwrap();
            assert_eq!(
                value,
                serde_json::json!({ "format_version": 1, "instance_id": "11111111-1111-4111-8111-111111111111",
                "action": "open", "target": "usage" })
            );
            stream
                .write_all(b"{\"format_version\":1,\"ok\":true}\n")
                .unwrap();
        });
        assert_eq!(open_at(&f.repo, &f.home, Target::Usage), Ok(()));
        worker.join().unwrap();
    }

    #[test]
    fn unsafe_missing_stale_or_wrong_project_endpoints_fail_closed() {
        let f = Fixture::new();
        let _listener = f.listener();
        let original = fs::read(&f.file).unwrap();
        fs::set_permissions(&f.file, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(open_at(&f.repo, &f.home, Target::Project).is_err());
        fs::remove_file(&f.file).unwrap();
        symlink("missing", &f.file).unwrap();
        assert!(open_at(&f.repo, &f.home, Target::Project).is_err());
        fs::remove_file(&f.file).unwrap();
        fs::write(&f.file, &original).unwrap();
        fs::set_permissions(&f.file, fs::Permissions::from_mode(0o600)).unwrap();
        let mut descriptor: serde_json::Value = serde_json::from_slice(&original).unwrap();
        descriptor["repo_root"] = serde_json::json!("/another/project");
        fs::write(&f.file, descriptor.to_string()).unwrap();
        assert!(open_at(&f.repo, &f.home, Target::Project).is_err());
        fs::write(&f.file, &original).unwrap();
        fs::set_permissions(&f.socket, fs::Permissions::from_mode(0o666)).unwrap();
        assert!(open_at(&f.repo, &f.home, Target::Project).is_err());
        fs::remove_file(&f.file).unwrap();
        assert!(open_at(&f.repo, &f.home, Target::Project).is_err());
    }

    #[test]
    fn a_fifo_descriptor_settles_without_a_writer() {
        use std::ffi::CString;
        use std::fs::OpenOptions;
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::OpenOptionsExt;

        let f = Fixture::new();
        fs::remove_file(&f.file).unwrap();
        let name = CString::new(f.file.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let (sender, receiver) = mpsc::sync_channel(1);
        let repo = f.repo.clone();
        let home = f.home.clone();
        let worker = std::thread::spawn(move || {
            let result = open_at(&repo, &home, Target::Project);
            sender.send(result).unwrap();
        });
        let settled = receiver.recv_timeout(Duration::from_millis(500));
        // Rescue the old blocking-open implementation before asserting, so a
        // regression fails without stranding the test's callback thread.
        let rescue = if settled.is_err() {
            OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&f.file)
                .ok()
        } else {
            None
        };
        if settled.is_err() {
            let _ = receiver.recv_timeout(Duration::from_secs(1));
        }
        assert_eq!(
            settled,
            Ok(Err(())),
            "FIFO descriptor must be rejected before waiting for any writer"
        );
        worker.join().unwrap();
        drop(rescue);
    }

    #[test]
    fn an_opener_failure_or_extra_reply_fields_never_become_a_url_or_success() {
        for reply in [
            "{\"format_version\":1,\"error\":\"failed with private argv\"}\n",
            "{\"format_version\":1,\"ok\":true,\"url\":\"private\"}\n",
        ] {
            let f = Fixture::new();
            let listener = f.listener();
            let worker = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut input = String::new();
                BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut input)
                    .unwrap();
                stream.write_all(reply.as_bytes()).unwrap();
            });
            assert!(open_at(&f.repo, &f.home, Target::Project).is_err());
            worker.join().unwrap();
        }
    }

    #[test]
    fn a_trickling_host_callback_has_one_absolute_deadline() {
        let f = Fixture::new();
        let listener = f.listener();
        let worker = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut input = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut input)
                .unwrap();
            for _ in 0..12 {
                std::thread::sleep(Duration::from_millis(200));
                if stream.write_all(b" ").is_err() {
                    break;
                }
            }
        });
        let start = Instant::now();
        assert!(open_at(&f.repo, &f.home, Target::Project).is_err());
        assert!(start.elapsed() < Duration::from_millis(2200));
        worker.join().unwrap();
    }
}
