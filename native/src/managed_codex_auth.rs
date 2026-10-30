//! Host-only selected-key bootstrap. No thread/model RPC, login replay, writable
//! readiness marker, secret Debug/Serde, inherited credentials or disk auth.
use super::{AgentLinkLaunch, OwnedPeerProcess};
use crate::managed_launch::ManagedCodexApiKey;
use rudder_engine::execution::handoff::BackendProcessWitness;
use serde_json::{json, Value};
use std::{io, mem, os::{fd::{AsRawFd, FromRawFd, OwnedFd}, unix::{ffi::OsStrExt, net::UnixStream}},
    path::Path, sync::{Arc, atomic::{AtomicBool, Ordering}}, time::{Duration, Instant}};
use tungstenite::{handshake::HandshakeError, Message, WebSocket};

const AUTH_TIMEOUT: Duration = Duration::from_secs(15);
const FRAME_LIMIT: usize = 64 * 1024;

/// Static failures only: provider errors and panic payloads may echo secrets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ManagedCodexAuthError { Cancelled, Deadline, Binding, Transport, Protocol, Storage, Rejected, Account }
impl std::fmt::Display for ManagedCodexAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { write!(f, "managed Codex authentication failed ({:?})", self) }
}
impl std::error::Error for ManagedCodexAuthError {}
type Result<T> = std::result::Result<T, ManagedCodexAuthError>;

pub(crate) fn authenticate_managed_codex_api_key(launch: &AgentLinkLaunch, process: Arc<BackendProcessWitness>,
    key: ManagedCodexApiKey, cancelled: &AtomicBool, deadline: Instant) -> Result<()> {
    if launch.format_version != 1 || launch.backend != "codex"
        || !process.matches_attempt(&launch.run_id, &launch.attempt_id, "codex-app-server")
        || !process.authenticated_start_required() { return Err(ManagedCodexAuthError::Binding); }
    bootstrap(&launch.socket_path, process.as_ref(), &key, cancelled, deadline.min(Instant::now() + AUTH_TIMEOUT))
}
fn current(process: &dyn OwnedPeerProcess, cancelled: &AtomicBool, deadline: Instant) -> Result<()> {
    if cancelled.load(Ordering::Acquire) { return Err(ManagedCodexAuthError::Cancelled); }
    if Instant::now() >= deadline { return Err(ManagedCodexAuthError::Deadline); }
    process.check_current().map_err(|_|ManagedCodexAuthError::Binding)
}
fn wait(fd: i32, events: i16, process: &dyn OwnedPeerProcess, cancelled: &AtomicBool, deadline: Instant) -> Result<i16> {
    current(process, cancelled, deadline)?;
    let mut event = libc::pollfd { fd, events, revents:0 };
    if unsafe { libc::poll(&mut event, 1, 20) } < 0 {
        if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted { return Err(ManagedCodexAuthError::Transport); }
        event.revents = 0;
    }
    current(process, cancelled, deadline)?;
    Ok(event.revents)
}
fn retryable(error: &tungstenite::Error) -> bool {
    matches!(error, tungstenite::Error::Io(error) if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted))
}
fn connect(path: &Path, process: &dyn OwnedPeerProcess, cancelled: &AtomicBool, deadline: Instant) -> Result<WebSocket<UnixStream>> {
    current(process, cancelled, deadline)?;
    let bytes = path.as_os_str().as_bytes();
    let mut address: libc::sockaddr_un = unsafe { mem::zeroed() };
    if !path.is_absolute() || bytes.contains(&0) || bytes.len() >= address.sun_path.len() { return Err(ManagedCodexAuthError::Binding); }
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (target, byte) in address.sun_path.iter_mut().zip(bytes) { *target = *byte as libc::c_char; }
    let stream = 'startup: loop {
    current(process, cancelled, deadline)?;
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK, 0) };
    if fd < 0 { return Err(ManagedCodexAuthError::Transport); }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let size = (mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1) as libc::socklen_t;
    if unsafe { libc::connect(fd.as_raw_fd(), (&address as *const libc::sockaddr_un).cast(), size) } < 0 {
        let error = io::Error::last_os_error().raw_os_error();
        if matches!(error, Some(libc::ENOENT | libc::ECONNREFUSED | libc::EAGAIN)) {
            drop(fd); std::thread::sleep(Duration::from_millis(20)); continue 'startup;
        }
        if error != Some(libc::EINPROGRESS) { return Err(ManagedCodexAuthError::Transport); }
        loop {
            let events = wait(fd.as_raw_fd(), libc::POLLOUT, process, cancelled, deadline)?;
            if events & (libc::POLLOUT | libc::POLLERR | libc::POLLHUP) == 0 { continue; }
            let mut error: libc::c_int = 0; let mut length = mem::size_of_val(&error) as libc::socklen_t;
            if unsafe { libc::getsockopt(fd.as_raw_fd(), libc::SOL_SOCKET, libc::SO_ERROR, (&mut error as *mut libc::c_int).cast(), &mut length) } != 0 {
                return Err(ManagedCodexAuthError::Transport);
            }
            if error == 0 && events & libc::POLLOUT != 0 { break; }
            if matches!(error, libc::ECONNREFUSED | libc::EAGAIN) { drop(fd); std::thread::sleep(Duration::from_millis(20)); continue 'startup; }
            if error != libc::EINPROGRESS { return Err(ManagedCodexAuthError::Transport); }
        }
    }
    break UnixStream::from(fd);
    };
    // Verify before HTTP/WebSocket bytes, and again before any credential send.
    process.verify_peer(&stream).map_err(|_|ManagedCodexAuthError::Binding)?;
    let raw = stream.as_raw_fd();
    let config = tungstenite::protocol::WebSocketConfig { max_message_size:Some(FRAME_LIMIT), max_frame_size:Some(FRAME_LIMIT),
        write_buffer_size:0, max_write_buffer_size:2*FRAME_LIMIT, ..Default::default() };
    let mut handshake = tungstenite::client::client_with_config("ws://localhost/", stream, Some(config));
    loop {
        match handshake {
            Ok((socket, _)) => return Ok(socket),
            Err(HandshakeError::Interrupted(next)) => {
                wait(raw, libc::POLLIN, process, cancelled, deadline)?;
                handshake = next.handshake();
            }
            Err(_) => return Err(ManagedCodexAuthError::Transport),
        }
    }
}
fn send(socket: &mut WebSocket<UnixStream>, value: Value, process: &dyn OwnedPeerProcess, cancelled: &AtomicBool, deadline: Instant) -> Result<()> {
    current(process, cancelled, deadline)?;
    let bytes = serde_json::to_string(&value).map_err(|_|ManagedCodexAuthError::Protocol)?;
    if bytes.len() > FRAME_LIMIT { return Err(ManagedCodexAuthError::Protocol); }
    // write exactly once. WouldBlock continues flushing this same buffered
    // frame; it never queues another login request after an uncertain write.
    if let Err(error) = socket.write(Message::Text(bytes)) { if !retryable(&error) { return Err(ManagedCodexAuthError::Transport); } }
    loop { current(process, cancelled, deadline)?;
        match socket.flush() { Ok(()) => return Ok(()), Err(error) if retryable(&error) => { wait(socket.get_ref().as_raw_fd(), libc::POLLOUT, process, cancelled, deadline)?; }
            Err(_) => return Err(ManagedCodexAuthError::Transport) }
    }
}
fn call(socket: &mut WebSocket<UnixStream>, id: u64, method: &str, params: Value,
    process: &dyn OwnedPeerProcess, cancelled: &AtomicBool, deadline: Instant, frames: &mut usize) -> Result<Value> {
    send(socket, json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}), process, cancelled, deadline)?;
    loop {
        current(process, cancelled, deadline)?;
        let message = match socket.read() {
            Ok(Message::Text(text)) => serde_json::from_str::<Value>(&text).map_err(|_|ManagedCodexAuthError::Protocol)?,
            Ok(Message::Binary(bytes)) => serde_json::from_slice(&bytes).map_err(|_|ManagedCodexAuthError::Protocol)?,
            Ok(Message::Close(_)) => return Err(ManagedCodexAuthError::Transport),
            Ok(_) => { *frames += 1; if *frames > 128 { return Err(ManagedCodexAuthError::Protocol); } continue; }
            Err(error) if retryable(&error) => { wait(socket.get_ref().as_raw_fd(), libc::POLLIN, process, cancelled, deadline)?; continue; }
            Err(_) => return Err(ManagedCodexAuthError::Transport),
        };
        *frames += 1; if *frames > 128 { return Err(ManagedCodexAuthError::Protocol); }
        current(process, cancelled, deadline)?;
        if message.get("method").is_none() && message.get("id").and_then(Value::as_u64) == Some(id) {
            if message.get("error").is_some() { return Err(ManagedCodexAuthError::Rejected); }
            return message.get("result").cloned().ok_or(ManagedCodexAuthError::Protocol);
        }
    }
}
fn bootstrap(path: &Path, process: &dyn OwnedPeerProcess, key: &ManagedCodexApiKey, cancelled: &AtomicBool, deadline: Instant) -> Result<()> {
    let mut socket = connect(path, process, cancelled, deadline)?;
    let mut frames = 0;
    let initialized = call(&mut socket, 1, "initialize", json!({"clientInfo":{"name":"rudder_agent_link","version":env!("CARGO_PKG_VERSION")},
        "capabilities":{"experimentalApi":true}}), process, cancelled, deadline, &mut frames)?;
    if !initialized.get("userAgent").and_then(Value::as_str).is_some_and(super::codex_peer_controls_supported) { return Err(ManagedCodexAuthError::Protocol); }
    send(&mut socket, json!({"jsonrpc":"2.0","method":"initialized"}), process, cancelled, deadline)?;
    let config = call(&mut socket, 2, "config/read", json!({"includeLayers":false}), process, cancelled, deadline, &mut frames)?;
    if config.pointer("/config/cli_auth_credentials_store").and_then(Value::as_str) != Some("ephemeral") { return Err(ManagedCodexAuthError::Storage); }
    process.verify_peer(socket.get_ref()).map_err(|_|ManagedCodexAuthError::Binding)?;
    let login = call(&mut socket, 3, "account/login/start", json!({"type":"apiKey","apiKey":key.expose_for_login()}), process, cancelled, deadline, &mut frames)?;
    if login.get("type").and_then(Value::as_str) != Some("apiKey") { return Err(ManagedCodexAuthError::Rejected); }
    let account = call(&mut socket, 4, "account/read", json!({"refreshToken":false}), process, cancelled, deadline, &mut frames)?;
    if account.pointer("/account/type").and_then(Value::as_str) != Some("apiKey") || account.get("requiresOpenaiAuth").and_then(Value::as_bool) != Some(true) {
        return Err(ManagedCodexAuthError::Account);
    }
    current(process, cancelled, deadline)
}

#[cfg(test)]
#[path = "managed_codex_auth_tests.rs"]
mod tests;
