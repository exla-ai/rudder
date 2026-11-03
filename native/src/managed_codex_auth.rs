//! Host-only selected-key bootstrap. No thread/model RPC, login replay, writable
//! readiness marker, secret Debug/Serde, inherited credentials or disk auth.
use super::{AgentLinkLaunch, OwnedPeerProcess};
use crate::managed_launch::{ManagedCodexApiKey, ManagedCodexCredential, CodexOAuthAccessSnapshotV1};
use rudder_engine::execution::handoff::BackendProcessWitness;
use serde_json::{json, Value};
use std::{io, mem, os::{fd::{AsRawFd, FromRawFd, OwnedFd}, unix::{ffi::OsStrExt, net::UnixStream}},
    path::Path, sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}}, time::{Duration, Instant}};
use tungstenite::{handshake::HandshakeError, Message, WebSocket};

const AUTH_TIMEOUT: Duration = Duration::from_secs(15);
const FRAME_LIMIT: usize = 64 * 1024;

/// Static failures only: provider errors and panic payloads may echo secrets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ManagedCodexAuthError { Cancelled, Deadline, Binding, Transport, Protocol, Storage, Rejected, Account, Renewal }
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
/// The SAME authenticated socket and verified backend remain held in Live and
/// then its TaskDrain. Only existing off-App workers lock or poll this owner.
/// A failed/expired snapshot is absorbing; it never loads or renews credentials.
pub(crate) struct ManagedCodexAuthSession {
    held: Mutex<SnapshotConnection>, stop: AtomicBool,
}
struct SnapshotConnection {
    process: Arc<BackendProcessWitness>, socket: Option<WebSocket<UnixStream>>, expires_at_ms:u64,
}
impl ManagedCodexAuthSession {
    pub(crate) fn new(process:Arc<BackendProcessWitness>) -> Arc<Self> {
        Arc::new(Self { held:Mutex::new(SnapshotConnection {process,socket:None,expires_at_ms:0}),stop:AtomicBool::new(false) })
    }
    pub(crate) fn stop_requested(&self)->bool { self.stop.load(Ordering::Acquire) }
    pub(crate) fn refuse(&self) { self.stop.store(true,Ordering::Release); }
    pub(crate) fn poll_off_app(&self) {
        if self.stop_requested() { return; }
        let mut held=match self.held.lock() {Ok(held)=>held,Err(_)=>{self.refuse();return;}};
        let process=held.process.clone(); let expiry=held.expires_at_ms;
        if process.check_current().is_err() || now_ms().is_none_or(|now|now>=expiry) {
            self.refuse();return;
        }
        let Some(socket)=held.socket.as_mut() else {self.refuse();return;};
        poll_snapshot_owned(socket,process.as_ref(),expiry,&self.stop);
    }
}
fn now_ms()->Option<u64> { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_millis().try_into().ok() }
/// Returns only a fixed denial, preserving the server's exact request identity.
/// There is no access token in this response and no successful renewal branch.
fn renewal_denial(message:&Value)->Result<Value> {
    if message.get("jsonrpc").and_then(Value::as_str)!=Some("2.0")
        || message.get("method").and_then(Value::as_str)!=Some("account/chatgptAuthTokens/refresh") {
        return Err(ManagedCodexAuthError::Protocol);
    }
    let id=message.get("id").filter(|id|id.as_i64().is_some() || id.as_u64().is_some()
        || id.as_str().is_some_and(|s|!s.is_empty() && s.len()<=256 && !s.chars().any(char::is_control)))
        .ok_or(ManagedCodexAuthError::Protocol)?;
    Ok(json!({"jsonrpc":"2.0","id":id,"error":{"code":-32000,"message":"managed OAuth access snapshot renewal required"}}))
}
fn poll_snapshot_owned(socket:&mut WebSocket<UnixStream>,process:&dyn OwnedPeerProcess,expiry:u64,stop:&AtomicBool) {
    if stop.load(Ordering::Acquire) {return;}
    if now_ms().is_none_or(|now|now>=expiry) || poll_snapshot(socket,process).is_err() {stop.store(true,Ordering::Release);}
}
fn poll_snapshot(socket:&mut WebSocket<UnixStream>, process:&dyn OwnedPeerProcess)->Result<()> {
    process.check_current().map_err(|_|ManagedCodexAuthError::Binding)?;
    for _ in 0..32 {
        let message=match socket.read() {
            Ok(Message::Text(text))=>serde_json::from_str::<Value>(&text).map_err(|_|ManagedCodexAuthError::Protocol)?,
            Ok(Message::Binary(bytes))=>serde_json::from_slice::<Value>(&bytes).map_err(|_|ManagedCodexAuthError::Protocol)?,
            Ok(Message::Close(_))=>return Err(ManagedCodexAuthError::Transport),
            Ok(_)=>continue,
            Err(error) if retryable(&error)=>return Ok(()),
            Err(_)=>return Err(ManagedCodexAuthError::Transport),
        };
        if message.get("method").and_then(Value::as_str)==Some("account/chatgptAuthTokens/refresh") {
            let denial=renewal_denial(&message)?;
            // One nonblocking write, with no retry/requeue. Any uncertainty still
            // requests the original task stop and retains this same socket.
            socket.write(Message::Text(denial.to_string())).map_err(|_|ManagedCodexAuthError::Transport)?;
            socket.flush().map_err(|_|ManagedCodexAuthError::Transport)?;
            return Err(ManagedCodexAuthError::Renewal);
        }
        // No other server request is silently acknowledged.
        if message.get("id").is_some() {return Err(ManagedCodexAuthError::Protocol);}
    }
    Err(ManagedCodexAuthError::Protocol)
}
pub(crate) fn authenticate_managed_codex(launch:&AgentLinkLaunch,process:Arc<BackendProcessWitness>,
    credential:ManagedCodexCredential,session:Option<&ManagedCodexAuthSession>,cancelled:&AtomicBool,deadline:Instant)->Result<()> {
    match credential {
        ManagedCodexCredential::ApiKey(key)=>authenticate_managed_codex_api_key(launch,process,key,cancelled,deadline),
        ManagedCodexCredential::AccessSnapshot(snapshot)=>{
            let session=session.ok_or(ManagedCodexAuthError::Binding)?;
            let result=(|| {
                if launch.format_version!=1 || launch.backend!="codex"
                    || !process.matches_attempt(&launch.run_id,&launch.attempt_id,"codex-app-server")
                    || !process.authenticated_start_required() {return Err(ManagedCodexAuthError::Binding);}
                let mut held=session.held.lock().map_err(|_|ManagedCodexAuthError::Binding)?;
                if !Arc::ptr_eq(&held.process,&process) || held.socket.is_some() || session.stop_requested() {return Err(ManagedCodexAuthError::Binding);}
                held.expires_at_ms=snapshot.expires_at_ms();
                let remaining=snapshot.expires_at_ms().checked_sub(now_ms().ok_or(ManagedCodexAuthError::Renewal)?)
                    .filter(|remaining|*remaining>0).ok_or(ManagedCodexAuthError::Renewal)?;
                let deadline=deadline.min(Instant::now()+AUTH_TIMEOUT).min(Instant::now()+Duration::from_millis(remaining));
                held.socket=Some(connect(&launch.socket_path,process.as_ref(),cancelled,deadline)?);
                bootstrap_snapshot(held.socket.as_mut().unwrap(),process.as_ref(),&snapshot,cancelled,deadline)
            })();
            if result.is_err() {session.refuse();} result
        }
    }
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
// Protocol serialization is transient; zero the owned JSON copies before the
// WebSocket takes its bounded frame. Selected credential owners never clone.
fn erase_json_strings(value:&mut Value) {
    match value {Value::String(text)=>{for byte in unsafe{text.as_bytes_mut()} {unsafe{std::ptr::write_volatile(byte,0);}}},
        Value::Array(values)=>for value in values {erase_json_strings(value);},
        Value::Object(values)=>for value in values.values_mut() {erase_json_strings(value);},_=>{}}
    std::sync::atomic::compiler_fence(Ordering::SeqCst);
}
struct ErasedJson(Value);
impl Drop for ErasedJson {fn drop(&mut self){erase_json_strings(&mut self.0);}}
fn send(socket: &mut WebSocket<UnixStream>, value: Value, process: &dyn OwnedPeerProcess, cancelled: &AtomicBool, deadline: Instant) -> Result<()> {
    let mut value=ErasedJson(value);
    current(process, cancelled, deadline)?;
    let encoded=serde_json::to_string(&value.0);
    erase_json_strings(&mut value.0);
    let bytes=encoded.map_err(|_|ManagedCodexAuthError::Protocol)?;
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
        if message.get("method").and_then(Value::as_str)==Some("account/chatgptAuthTokens/refresh") {
            let denial=renewal_denial(&message)?;
            send(socket,denial,process,cancelled,deadline)?;
            return Err(ManagedCodexAuthError::Renewal);
        }
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

fn bootstrap_snapshot(socket:&mut WebSocket<UnixStream>,process:&dyn OwnedPeerProcess,snapshot:&CodexOAuthAccessSnapshotV1,
    cancelled:&AtomicBool,deadline:Instant)->Result<()> {
    if now_ms().is_none_or(|now|now>=snapshot.expires_at_ms()) {return Err(ManagedCodexAuthError::Renewal);}
    let mut frames=0;
    let initialized=call(socket,1,"initialize",json!({"clientInfo":{"name":"rudder_agent_link","version":env!("CARGO_PKG_VERSION")},
        "capabilities":{"experimentalApi":true}}),process,cancelled,deadline,&mut frames)?;
    if !initialized.get("userAgent").and_then(Value::as_str).is_some_and(snapshot_protocol_supported) {return Err(ManagedCodexAuthError::Protocol);}
    send(socket,json!({"jsonrpc":"2.0","method":"initialized"}),process,cancelled,deadline)?;
    let config=call(socket,2,"config/read",json!({"includeLayers":false}),process,cancelled,deadline,&mut frames)?;
    if config.pointer("/config/cli_auth_credentials_store").and_then(Value::as_str)!=Some("ephemeral") {return Err(ManagedCodexAuthError::Storage);}
    process.verify_peer(socket.get_ref()).map_err(|_|ManagedCodexAuthError::Binding)?;
    if now_ms().is_none_or(|now|now>=snapshot.expires_at_ms()) {return Err(ManagedCodexAuthError::Renewal);}
    let mut params=json!({"type":"chatgptAuthTokens","accessToken":snapshot.access(),"chatgptAccountId":snapshot.account()});
    if let Some(plan)=snapshot.plan() {params["chatgptPlanType"]=Value::String(plan.into());}
    let login=call(socket,3,"account/login/start",params,process,cancelled,deadline,&mut frames)?;
    if login.get("type").and_then(Value::as_str)!=Some("chatgptAuthTokens") {return Err(ManagedCodexAuthError::Rejected);}
    let account=call(socket,4,"account/read",json!({"refreshToken":false}),process,cancelled,deadline,&mut frames)?;
    let observed=account.get("account").and_then(Value::as_object).ok_or(ManagedCodexAuthError::Account)?;
    if observed.get("type").and_then(Value::as_str)!=Some("chatgpt")
        || !observed.get("email").is_some_and(|v|v.is_null()||v.as_str().is_some())
        || !observed.get("planType").and_then(Value::as_str).is_some_and(valid_plan)
        || account.get("requiresOpenaiAuth").and_then(Value::as_bool)!=Some(true) {return Err(ManagedCodexAuthError::Account);}
    if now_ms().is_none_or(|now|now>=snapshot.expires_at_ms()) {return Err(ManagedCodexAuthError::Renewal);}
    current(process,cancelled,deadline)
}
fn snapshot_protocol_supported(user_agent:&str)->bool {
    super::codex_peer_controls_supported(user_agent) && user_agent.split_whitespace().next()
        .and_then(|product|product.split_once('/')).is_some_and(|(_,version)|version=="0.166.0")
}
fn valid_plan(plan:&str)->bool {matches!(plan,"free"|"go"|"plus"|"pro"|"prolite"|"promax"|"team"|"self_serve_business_prolite"|"self_serve_business_usage_based"|"business"|"ent26"|"enterprise_cbp_automation"|"enterprise_cbp_usage_based"|"enterprise"|"edu"|"edu_plus"|"edu_pro"|"sci"|"sci_plus"|"unknown")}

#[cfg(test)]
#[path = "managed_codex_auth_tests.rs"]
mod tests;

#[cfg(test)]
#[path="managed_codex_snapshot_auth_tests.rs"]
mod snapshot_tests;
