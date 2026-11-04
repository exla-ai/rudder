//! Trusted Linux host-only profile helper. Every API blocks OFF App; caller keeps
//! the original admission/budget until actual return, including post-kill wait.
#![cfg(target_os = "linux")]
use crate::{Backend, managed_task_launch::{ManagedTaskOptIn, SelectedTaskProfile}};
use rudder_engine::execution::finalization_budget::FinalizationBudget;
use serde::Deserialize;
use std::{fs, io, os::{fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd}, unix::{fs::MetadataExt, process::CommandExt}},
    path::{Component, Path, PathBuf}, process::{Child, Command, ExitStatus, Stdio},
    sync::atomic::{AtomicBool, Ordering}, time::Duration};

const KEY_LIMIT: usize = 4092;
const SNAPSHOT_LIMIT:usize=65536;
const CONTROL_LIMIT: usize = 4096;
const STDERR_LIMIT: usize = 2048;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ManagedProfileLoadError { Cancelled, Deadline, Runtime, Selection, Spawn, Transport,
    Overflow, Protocol, Rejected, UnsupportedOAuth, UnsupportedToken, Credential }
impl std::fmt::Display for ManagedProfileLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { write!(f, "private managed profile load failed ({self:?})") }
}
impl std::error::Error for ManagedProfileLoadError {}
type Result<T> = std::result::Result<T, ManagedProfileLoadError>;
/// Trusted bootstrap supplies actual immutable host runtime paths and the normal
/// private profile directory. DATA only; no serialized authority constructor.
pub(crate) struct ManagedProfileLoaderRuntime { pub node: PathBuf, pub helper: PathBuf, pub rudder_home: PathBuf }
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ManagedProfileCredentialKind { ApiKeyV1, OAuthV1, TokenV1 }
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ManagedProfileMetadata { pub profile_id: String, pub backend: Backend,
    pub kind: ManagedProfileCredentialKind, provider: String }
impl ManagedProfileMetadata {
    pub(crate) fn supports_codex_access_snapshot(&self)->bool {
        self.backend==Backend::Codex && self.kind==ManagedProfileCredentialKind::OAuthV1 && self.provider=="openai-codex"
    }
}

/// Loaded credential owner plus its validated non-secret model selection.
/// The task-launch caller keeps the selection through session initialization
/// and records bind_session's receipt with the original execution identity.
pub(crate) struct SelectedManagedModelProfile {
    profile: SelectedTaskProfile,
    selection: serde_json::Value,
}
impl SelectedManagedModelProfile {
    pub(crate) fn into_parts(self) -> (SelectedTaskProfile, serde_json::Value) {
        (self.profile, self.selection)
    }
}

/// OFF App. Use with capabilities from the selected backend's actual protocol.
/// Retains explicit model/effort while the existing private one-shot loader
/// remains the only credential handoff. No session receipt is minted by IO.
pub(crate) fn load_selected_managed_model_profile(runtime: &ManagedProfileLoaderRuntime,
    selected: &ManagedTaskOptIn, selection: &serde_json::Value,
    cancelled: &AtomicBool, budget: &FinalizationBudget) -> Result<SelectedManagedModelProfile> {
    check(budget, cancelled)?;
    rudder_engine::execution::ExecutionProfileV4::validate_selected_model(selection)
        .map_err(|_| ManagedProfileLoadError::Selection)?;
    if selection["profile_id"] != selected.profile_id() || selection["backend"] != backend_name(selected.backend())? {
        return Err(ManagedProfileLoadError::Selection);
    }
    let metadata = inspect_selected_managed_task_profile(runtime, selected.profile_id(), selected.backend(), cancelled, budget)?;
    let kind = match metadata.kind { ManagedProfileCredentialKind::ApiKeyV1 => "api_key",
        ManagedProfileCredentialKind::OAuthV1 => "oauth", ManagedProfileCredentialKind::TokenV1 => "token" };
    if selection["provider"] != metadata.provider || selection["credential_kind"] != kind {
        return Err(ManagedProfileLoadError::Selection);
    }
    // The inner load re-inspects before handoff; verify metadata again against
    // this original snapshot to reject profile type/provider races as well.
    let mut profile = load_profile_matching(runtime, selected, cancelled, budget, Some(&metadata))?;
    profile.retain_model_selection(selection.clone()).map_err(|_|ManagedProfileLoadError::Selection)?;
    check(budget, cancelled)?;
    Ok(SelectedManagedModelProfile { profile, selection: selection.clone() })
}
/// Discover capabilities through the actual protected installed program, then
/// load only the explicitly selected credential. No backend turn is started.
pub(crate) fn discover_and_load_selected_model(runtime:&ManagedProfileLoaderRuntime,
    selected:&ManagedTaskOptIn, model:&str, program:&Path, cancelled:&AtomicBool,
    budget:&FinalizationBudget)->Result<SelectedTaskProfile> {
    if selected.backend()!=Backend::Codex || !exact(program) || model.is_empty() || model.len()>256 || model.chars().any(char::is_control) {
        return Err(ManagedProfileLoadError::Selection);
    }
    let program=program.to_str().ok_or(ManagedProfileLoadError::Runtime)?;
    let collected=run_verb_args(runtime,selected.profile_id(),selected.backend(),"discover-model",&[model,program],cancelled,budget)?;
    #[derive(Deserialize)] #[serde(deny_unknown_fields)]
    struct Reply {ok:bool, selection:serde_json::Value}
    if !collected.status.success() || !collected.error.0.is_empty() || !collected.key.0.is_empty() {return Err(ManagedProfileLoadError::Rejected);}
    let reply:Reply=serde_json::from_slice(&collected.output.0).map_err(|_|ManagedProfileLoadError::Protocol)?;
    if !reply.ok || reply.selection["requested_model"]!=model {return Err(ManagedProfileLoadError::Selection);}
    let loaded=load_selected_managed_model_profile(runtime,selected,&reply.selection,cancelled,budget)?;
    Ok(loaded.into_parts().0)
}
fn check(budget: &FinalizationBudget, cancelled: &AtomicBool) -> Result<()> {
    if cancelled.load(Ordering::Acquire) { return Err(ManagedProfileLoadError::Cancelled); }
    budget.remaining().map(|_| ()).map_err(|_| ManagedProfileLoadError::Deadline)
}
fn backend_name(backend: Backend) -> Result<&'static str> {
    match backend { Backend::Codex => Ok("codex"), Backend::Claude => Ok("claude"), _ => Err(ManagedProfileLoadError::Selection) }
}
fn validate_selection(profile_id: &str, backend: Backend) -> Result<()> {
    backend_name(backend)?;
    if profile_id.is_empty() || profile_id.len() > 256 || profile_id.chars().any(char::is_control) {
        return Err(ManagedProfileLoadError::Selection);
    }
    Ok(())
}
fn exact(path: &Path) -> bool {
    path.is_absolute() && path.components().all(|p| matches!(p, Component::RootDir | Component::Normal(_)))
        && path.canonicalize().is_ok_and(|p| p == path)
}
fn validate_runtime(runtime: &ManagedProfileLoaderRuntime) -> Result<()> {
    // Node decodes env as UTF-8; normal rudderHome() then trims RUDDER_HOME.
    // Reject any byte/trim translation that could silently select another store.
    let home_text = runtime.rudder_home.to_str().ok_or(ManagedProfileLoadError::Runtime)?;
    if home_text.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}') != home_text {
        return Err(ManagedProfileLoadError::Runtime);
    }
    let uid = unsafe { libc::geteuid() };
    for path in [&runtime.node, &runtime.helper, &runtime.rudder_home] {
        if !exact(path) { return Err(ManagedProfileLoadError::Runtime); }
        let m = fs::symlink_metadata(path).map_err(|_| ManagedProfileLoadError::Runtime)?;
        if m.mode() & 0o022 != 0 || (m.uid() != uid && m.uid() != 0) { return Err(ManagedProfileLoadError::Runtime); }
    }
    let node = fs::metadata(&runtime.node).map_err(|_| ManagedProfileLoadError::Runtime)?;
    let helper = fs::metadata(&runtime.helper).map_err(|_| ManagedProfileLoadError::Runtime)?;
    let home = fs::metadata(&runtime.rudder_home).map_err(|_| ManagedProfileLoadError::Runtime)?;
    if !node.is_file() || node.mode() & 0o111 == 0 || !helper.is_file() || !home.is_dir()
        || home.uid() != uid || home.mode() & 0o077 != 0 { return Err(ManagedProfileLoadError::Runtime); }
    Ok(())
}
fn erase(bytes: &mut [u8]) {
    for byte in bytes { unsafe { std::ptr::write_volatile(byte, 0); } }
    std::sync::atomic::compiler_fence(Ordering::SeqCst);
}
struct SecretBytes(Vec<u8>);
impl SecretBytes {
    fn new(limit: usize) -> Self { Self(Vec::with_capacity(limit)) }
    fn erase(&mut self) { erase(&mut self.0); self.0.clear(); }
}
impl Drop for SecretBytes { fn drop(&mut self) { self.erase(); } }
struct ReadChunk([u8; 1024]);
impl Drop for ReadChunk { fn drop(&mut self) { erase(&mut self.0); } }
struct Capture { fd: Option<OwnedFd>, bytes: SecretBytes, limit: usize }
impl Capture {
    fn new(fd: Option<OwnedFd>, limit: usize) -> Result<Self> {
        if let Some(fd) = &fd {
            let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
            if flags < 0 || unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
                return Err(ManagedProfileLoadError::Transport);
            }
        }
        Ok(Self { fd, bytes: SecretBytes::new(limit), limit })
    }
    fn drain(&mut self) -> Result<()> {
        let Some(fd) = &self.fd else { return Ok(()) };
        let mut chunk = ReadChunk([0; 1024]);
        let n = unsafe { libc::read(fd.as_raw_fd(), chunk.0.as_mut_ptr().cast(), chunk.0.len()) };
        if n == 0 { self.fd.take(); return Ok(()); }
        if n < 0 { return match io::Error::last_os_error().kind() {
            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted => Ok(()), _ => Err(ManagedProfileLoadError::Transport),
        }; }
        let n = n as usize;
        if n > self.limit - self.bytes.0.len() { return Err(ManagedProfileLoadError::Overflow); }
        self.bytes.0.extend_from_slice(&chunk.0[..n]); Ok(())
    }
}
/// A panic/error cannot detach the actual child. Pipes are declared after this
/// guard and close before its Drop kills/waits. Wait errors retain the charge.
struct ReapedChild { child: Child, status: Option<ExitStatus> }
impl ReapedChild {
    fn poll(&mut self) -> Result<()> {
        if self.status.is_none() { self.status = self.child.try_wait().map_err(|_| ManagedProfileLoadError::Transport)?; }
        Ok(())
    }
}
impl Drop for ReapedChild {
    fn drop(&mut self) {
        if self.status.is_some() { return; }
        // The live unreaped child owns this process group. Discovery may own
        // an app-server child; cancellation must terminate that child too.
        unsafe { libc::kill(-(self.child.id() as i32),libc::SIGKILL); }
        let _ = self.child.kill();
        loop { match self.child.wait() {
            Ok(status) => { self.status = Some(status); return; }
            Err(_) => std::thread::sleep(Duration::from_millis(10)),
        } }
    }
}
fn pipe() -> Result<(OwnedFd, OwnedFd)> {
    let mut raw = [-1; 2];
    if unsafe { libc::pipe2(raw.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) } != 0 { return Err(ManagedProfileLoadError::Transport); }
    let mut read = unsafe { OwnedFd::from_raw_fd(raw[0]) };
    let mut write = unsafe { OwnedFd::from_raw_fd(raw[1]) };
    // Preserve stdio if the host happened to have a closed FD0/1/2.
    for owned in [&mut read, &mut write] { if owned.as_raw_fd() < 3 {
        let copy = unsafe { libc::fcntl(owned.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
        if copy < 0 { return Err(ManagedProfileLoadError::Transport); }
        *owned = unsafe { OwnedFd::from_raw_fd(copy) };
    } }
    Ok((read, write))
}
struct Collected { output: SecretBytes, error: SecretBytes, key: SecretBytes, status: ExitStatus }
// Child-only, async-signal-safe FD operations. Parent ends stay CLOEXEC across
// concurrent execs; only this designated child's FD3 receives the writer.
unsafe fn inherit_writer(r: i32, w: i32) -> io::Result<()> {
    if w == 3 {
        if libc::fcntl(3, libc::F_SETFD, 0) < 0 { return Err(io::Error::last_os_error()); }
    } else if libc::dup2(w, 3) < 0 { return Err(io::Error::last_os_error()); }
    if r != 3 { libc::close(r); } // dup2 already replaced a reader originally at3.
    if w != 3 { libc::close(w); }
    Ok(())
}
fn run(runtime: &ManagedProfileLoaderRuntime, profile_id: &str, backend: Backend, handoff: bool,
    cancelled: &AtomicBool, budget: &FinalizationBudget) -> Result<Collected> {
    run_verb(runtime,profile_id,backend,if handoff {"handoff-api-key"} else {"inspect"},cancelled,budget)
}
fn run_verb(runtime:&ManagedProfileLoaderRuntime,profile_id:&str,backend:Backend,verb:&str,cancelled:&AtomicBool,budget:&FinalizationBudget)->Result<Collected> {
    run_verb_args(runtime,profile_id,backend,verb,&[],cancelled,budget)
}
fn run_verb_args(runtime:&ManagedProfileLoaderRuntime,profile_id:&str,backend:Backend,verb:&str,args:&[&str],cancelled:&AtomicBool,budget:&FinalizationBudget)->Result<Collected> {
    let handoff=verb.starts_with("handoff-");
    check(budget, cancelled)?; validate_selection(profile_id, backend)?; validate_runtime(runtime)?; check(budget, cancelled)?;
    let pipes = if handoff { Some(pipe()?) } else { None };
    let mut command = Command::new(&runtime.node);
    command.arg(&runtime.helper).arg(verb).arg(profile_id).arg(backend_name(backend)?).args(args).process_group(0)
        .env_clear().env("RUDDER_HOME", &runtime.rudder_home).current_dir(&runtime.rudder_home)
        .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some((reader, writer)) = &pipes {
        let r = reader.as_raw_fd(); let w = writer.as_raw_fd();
        unsafe { command.pre_exec(move || inherit_writer(r, w)); }
    }
    check(budget, cancelled)?;
    let child = command.spawn().map_err(|_| ManagedProfileLoadError::Spawn)?;
    let mut child = ReapedChild { child, status: None }; // Every later error/panic kills and waits.
    let key_fd = pipes.map(|(reader, writer)| { drop(writer); reader });
    let out = child.child.stdout.take().ok_or(ManagedProfileLoadError::Transport)?;
    let err = child.child.stderr.take().ok_or(ManagedProfileLoadError::Transport)?;
    let mut output = Capture::new(Some(unsafe { OwnedFd::from_raw_fd(out.into_raw_fd()) }), CONTROL_LIMIT)?;
    let mut error = Capture::new(Some(unsafe { OwnedFd::from_raw_fd(err.into_raw_fd()) }), STDERR_LIMIT)?;
    let mut key = Capture::new(key_fd, if verb=="handoff-codex-oauth-access-snapshot" {SNAPSHOT_LIMIT} else {KEY_LIMIT+4})?;
    loop {
        check(budget, cancelled)?;
        for channel in [&mut output, &mut error, &mut key] { check(budget, cancelled)?; channel.drain()?; }
        child.poll()?;
        if output.fd.is_none() && error.fd.is_none() && key.fd.is_none() && child.status.is_some() { break; }
        let mut events = [&output, &error, &key].map(|c| libc::pollfd {
            fd: c.fd.as_ref().map_or(-1, AsRawFd::as_raw_fd), events: libc::POLLIN | libc::POLLHUP, revents: 0 });
        let millis = budget.phase_limit(Duration::from_millis(10)).map_err(|_| ManagedProfileLoadError::Deadline)?.as_millis() as i32;
        if unsafe { libc::poll(events.as_mut_ptr(), events.len() as libc::nfds_t, millis) } < 0
            && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted { return Err(ManagedProfileLoadError::Transport); }
    }
    check(budget, cancelled)?;
    Ok(Collected { output: output.bytes, error: error.bytes, key: key.bytes, status: child.status.clone().unwrap() })
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct WireMetadata { format_version: u32, profile_id: String, backend: String, credential_kind: String, provider: String }
impl Drop for WireMetadata { fn drop(&mut self) { for s in [&mut self.profile_id, &mut self.backend, &mut self.credential_kind, &mut self.provider] { unsafe { erase(s.as_bytes_mut()); } } } }
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireError { code: String, message: serde::de::IgnoredAny }
impl Drop for WireError { fn drop(&mut self) { unsafe { erase(self.code.as_bytes_mut()); } } }
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireReply { ok: bool, metadata: Option<WireMetadata>, error: Option<WireError> }
fn metadata(collected: &Collected, profile_id: &str, backend: Backend) -> Result<ManagedProfileMetadata> {
    if !collected.error.0.is_empty() { return Err(ManagedProfileLoadError::Protocol); }
    let reply: WireReply = serde_json::from_slice(&collected.output.0).map_err(|_| ManagedProfileLoadError::Protocol)?;
    if !reply.ok {
        if collected.status.success() || reply.metadata.is_some() { return Err(ManagedProfileLoadError::Protocol); }
        return Err(match reply.error.as_ref().map(|e| e.code.as_str()) {
            Some("UNSUPPORTED_OAUTH") => ManagedProfileLoadError::UnsupportedOAuth,
            Some("UNSUPPORTED_TOKEN") => ManagedProfileLoadError::UnsupportedToken,
            _ => ManagedProfileLoadError::Rejected,
        });
    }
    let m = reply.metadata.as_ref().ok_or(ManagedProfileLoadError::Protocol)?;
    if !collected.status.success() || reply.error.is_some() || m.format_version != 1 || m.profile_id != profile_id
        || m.backend != backend_name(backend)? { return Err(ManagedProfileLoadError::Protocol); }
    let kind = match m.credential_kind.as_str() { "api_key" => ManagedProfileCredentialKind::ApiKeyV1,
        "oauth" => ManagedProfileCredentialKind::OAuthV1, "token" => ManagedProfileCredentialKind::TokenV1,
        _ => return Err(ManagedProfileLoadError::Protocol) };
    if !(backend == Backend::Claude && m.provider == "anthropic" || backend == Backend::Codex
        && (m.provider == "openai" || kind == ManagedProfileCredentialKind::OAuthV1 && m.provider == "openai-codex")) {
        return Err(ManagedProfileLoadError::Protocol);
    }
    Ok(ManagedProfileMetadata { profile_id: profile_id.to_owned(), backend, kind, provider: m.provider.clone() })
}
/// OFF App metadata only, usable BEFORE opt-in/project migration. Root rejects
/// OAuth/token kind explicitly; returned DATA cannot create a selected key owner.
pub(crate) fn inspect_selected_managed_task_profile(runtime: &ManagedProfileLoaderRuntime, profile_id: &str, backend: Backend,
    cancelled: &AtomicBool, budget: &FinalizationBudget) -> Result<ManagedProfileMetadata> {
    let collected = run(runtime, profile_id, backend, false, cancelled, budget)?;
    let inspected = metadata(&collected, profile_id, backend)?;
    check(budget, cancelled)?; Ok(inspected)
}
fn key(bytes: &[u8]) -> Result<&str> {
    if bytes.len() < 4 { return Err(ManagedProfileLoadError::Credential); }
    let size = u32::from_be_bytes(bytes[..4].try_into().unwrap()) as usize;
    if size == 0 || size > KEY_LIMIT || bytes.len() != size + 4 { return Err(ManagedProfileLoadError::Credential); }
    let key = std::str::from_utf8(&bytes[4..]).map_err(|_| ManagedProfileLoadError::Credential)?;
    if key.contains(['\0', '\r', '\n']) { return Err(ManagedProfileLoadError::Credential); }
    Ok(key)
}
/// OFF App. Same original deadline/cancel across reinspection and handoff; no
/// automatic effect retry, new secret wrapper or API-billing/profile fallback.
pub(crate) fn load_selected_managed_task_profile(runtime: &ManagedProfileLoaderRuntime, selected: &ManagedTaskOptIn,
    cancelled: &AtomicBool, budget: &FinalizationBudget) -> Result<SelectedTaskProfile> {
    load_profile_matching(runtime, selected, cancelled, budget, None)
}
fn load_profile_matching(runtime: &ManagedProfileLoaderRuntime, selected: &ManagedTaskOptIn,
    cancelled: &AtomicBool, budget: &FinalizationBudget, original: Option<&ManagedProfileMetadata>) -> Result<SelectedTaskProfile> {
    let expected = inspect_selected_managed_task_profile(runtime, selected.profile_id(), selected.backend(), cancelled, budget)?;
    if original.is_some_and(|original| original != &expected) { return Err(ManagedProfileLoadError::Selection); }
    let snapshot=match expected.kind {
        ManagedProfileCredentialKind::OAuthV1 if selected.accepts_codex_snapshot() && expected.supports_codex_access_snapshot()=>true,
        ManagedProfileCredentialKind::OAuthV1=>return Err(ManagedProfileLoadError::UnsupportedOAuth),
        ManagedProfileCredentialKind::TokenV1=>return Err(ManagedProfileLoadError::UnsupportedToken),
        _ if selected.accepts_api_key()=>false,
        _=>return Err(ManagedProfileLoadError::Selection),
    };
    let mut collected = run_verb(runtime, selected.profile_id(), selected.backend(), if snapshot {"handoff-codex-oauth-access-snapshot"} else {"handoff-api-key"}, cancelled, budget)?;
    if metadata(&collected, selected.profile_id(), selected.backend())? != expected { return Err(ManagedProfileLoadError::Protocol); }
    check(budget, cancelled)?;
    let loaded = if snapshot {
        let now=std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(|_|ManagedProfileLoadError::Credential)?.as_millis();
        let snapshot=crate::managed_launch::CodexOAuthAccessSnapshotV1::from_frame(&collected.key.0,selected.profile_id(),
            now.try_into().map_err(|_|ManagedProfileLoadError::Credential)?).map_err(|_|ManagedProfileLoadError::Credential)?;
        SelectedTaskProfile::codex_access_snapshot(selected.clone(),snapshot)
    } else {SelectedTaskProfile::api_key(selected.clone(),key(&collected.key.0)?.to_owned())}.map_err(|_|ManagedProfileLoadError::Credential);
    collected.key.erase(); check(budget, cancelled)?; loaded
}

#[cfg(test)]
#[path = "managed_profile_loader_pipe_tests.rs"]
mod tests;
