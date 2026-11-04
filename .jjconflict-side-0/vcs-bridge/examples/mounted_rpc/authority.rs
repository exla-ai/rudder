//! Fixture authority lives in the OUTER controller, outside helper and guest.
//! This is deliberately not evidence of the production native broker path.
use super::wire;
use anyhow::{Result, ensure};
use rudder_engine::vfs::rpc::{AttemptAuthority, ObservationBinding};
use rudder_vcs_bridge::{AttemptBinding, vfs_provider};
use serde::{Deserialize, Serialize};
use std::fs;
use std::os::unix::{
    fs::PermissionsExt,
    net::{UnixListener, UnixStream},
};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::Duration;

#[derive(Clone, Serialize, Deserialize)]
pub struct Client {
    pub socket: PathBuf,
    pub token: String,
    pub observation: ObservationBinding,
    pub attempt: AttemptBinding,
}
#[derive(Serialize, Deserialize)]
struct Request {
    token: String,
    binding: ObservationBinding,
}
impl Client {
    fn check(&self, binding: &ObservationBinding) -> Result<()> {
        ensure!(
            *binding == self.observation,
            "fixture authority binding mismatch"
        );
        let mut stream = UnixStream::connect(&self.socket)?;
        wire::blocking_stream(&stream, Duration::from_secs(2))?;
        wire::send(
            &mut stream,
            &serde_json::to_vec(&Request {
                token: self.token.clone(),
                binding: binding.clone(),
            })?,
        )?;
        ensure!(
            wire::frame(&mut stream)? == b"authorized",
            "fixture authority denied stale attempt/epoch"
        );
        Ok(())
    }
}
impl AttemptAuthority for Client {
    fn validate(&self, binding: &ObservationBinding) -> Result<()> {
        self.check(binding)
    }
}
impl vfs_provider::AttemptAuthority for Client {
    fn validate(&self, binding: &AttemptBinding) -> Result<()> {
        ensure!(*binding == self.attempt, "fixture VCS attempt differs");
        self.check(&self.observation)
    }
}
pub struct Server {
    pub client: Client,
    pub active: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}
impl Server {
    pub fn bind(
        path: &Path,
        observation: ObservationBinding,
        attempt: AttemptBinding,
    ) -> Result<Self> {
        let listener = UnixListener::bind(path)?;
        listener.set_nonblocking(true)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        let token = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        let client = Client {
            socket: path.into(),
            token: token.clone(),
            observation: observation.clone(),
            attempt,
        };
        let active = Arc::new(AtomicBool::new(true));
        let stop = Arc::new(AtomicBool::new(false));
        let (a, s) = (active.clone(), stop.clone());
        let thread = thread::spawn(move || {
            while !s.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = (|| -> Result<()> {
                            wire::blocking_stream(&stream, Duration::from_secs(2))?;
                            let request: Request =
                                serde_json::from_slice(&wire::frame(&mut stream)?)?;
                            let allowed = a.load(Ordering::Acquire)
                                && request.token == token
                                && request.binding == observation;
                            wire::send(&mut stream, if allowed { b"authorized" } else { b"denied" })
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
            client,
            active,
            stop,
            thread: Some(thread),
        })
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = fs::remove_file(&self.client.socket);
    }
}
