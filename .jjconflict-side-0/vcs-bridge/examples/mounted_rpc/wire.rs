use anyhow::{Result, ensure};
use serde::{Serialize, de::DeserializeOwned};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

pub fn private(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}
pub fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let tmp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&tmp)?;
    file.write_all(&serde_json::to_vec(value)?)?;
    file.sync_all()?;
    fs::rename(&tmp, path)?;
    File::open(path.parent().unwrap())?.sync_all()?;
    Ok(())
}
pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let bytes = fs::read(path)?;
    ensure!(
        bytes.len() <= 1024 * 1024,
        "fixture configuration exceeds bound"
    );
    Ok(serde_json::from_slice(&bytes)?)
}
pub fn wait_file(path: &Path, seconds: u64) -> Result<()> {
    let end = Instant::now() + Duration::from_secs(seconds);
    while !path.exists() {
        ensure!(
            Instant::now() < end,
            "fixture readiness deadline: {}",
            path.display()
        );
        thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}
pub fn blocking_stream(stream: &UnixStream, read_timeout: Duration) -> Result<()> {
    // Darwin inherits the nonblocking listener flag through accept(). Framed
    // I/O below relies on blocking reads/writes with explicit finite timeouts.
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(read_timeout))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    Ok(())
}
pub fn frame(stream: &mut UnixStream) -> Result<Vec<u8>> {
    let mut prefix = [0; 4];
    stream.read_exact(&mut prefix)?;
    let size = u32::from_be_bytes(prefix) as usize;
    ensure!(size <= 1024 * 1024, "fixture frame exceeds bound");
    let mut bytes = vec![0; size];
    stream.read_exact(&mut bytes)?;
    Ok(bytes)
}
pub fn send(stream: &mut UnixStream, bytes: &[u8]) -> Result<()> {
    ensure!(bytes.len() <= 1024 * 1024, "fixture frame exceeds bound");
    stream.write_all(&(bytes.len() as u32).to_be_bytes())?;
    stream.write_all(bytes)?;
    Ok(())
}

/// Owned fixture fault injector. The production servers still enforce their
/// own framing, peer identity and attempt checks; no token is logged here.
pub struct ReplyProxy {
    pub path: PathBuf,
    pub drop_observe: Arc<AtomicBool>,
    pub drop_execute: Arc<AtomicBool>,
    pub observes: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}
impl ReplyProxy {
    pub fn bind(path: PathBuf, target: PathBuf) -> Result<Self> {
        let listener = UnixListener::bind(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let drop_observe = Arc::new(AtomicBool::new(false));
        let drop_execute = Arc::new(AtomicBool::new(false));
        let observes = Arc::new(AtomicUsize::new(0));
        let (s, o, x, count) = (
            stop.clone(),
            drop_observe.clone(),
            drop_execute.clone(),
            observes.clone(),
        );
        let handle = thread::spawn(move || {
            while !s.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut client, _)) => {
                        let _ = (|| -> Result<()> {
                            blocking_stream(&client, Duration::from_secs(2))?;
                            let request = frame(&mut client)?;
                            let json: serde_json::Value = serde_json::from_slice(&request)?;
                            let operation = json["operation"]["operation"].as_str().unwrap_or("");
                            let mut server = UnixStream::connect(&target)?;
                            // The managed command has a 90s execution budget.
                            blocking_stream(&server, Duration::from_secs(100))?;
                            send(&mut server, &request)?;
                            let response = frame(&mut server)?;
                            if operation == "observe" {
                                count.fetch_add(1, Ordering::AcqRel);
                                if o.swap(false, Ordering::AcqRel) {
                                    return Ok(());
                                }
                            }
                            if operation == "execute" && x.swap(false, Ordering::AcqRel) {
                                return Ok(());
                            }
                            send(&mut client, &response)
                        })();
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5))
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Self {
            path,
            drop_observe,
            drop_execute,
            observes,
            stop,
            thread: Some(handle),
        })
    }
}
impl Drop for ReplyProxy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
        let _ = fs::remove_file(&self.path);
    }
}
