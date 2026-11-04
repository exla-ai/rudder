//! Ignored Linux test of the EXISTING host adapter against actual Codex.
//! No managed witness, production path, paid model, or automatic acquisition.
use super::*;
use std::fs::File;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Mutex;

const ELF_SHA: &str = "b8a11fedf24b2dfb59a70c9e52b030f9a508dcd49cd2dd0d41956f4c785156b8";
const ELF_BYTES: u64 = 319_346_888;
const PROVIDER_SHA: &str = "a685dae3b3dbb0fee6777432ff7f0973ddb131405d708c6cbec05d204432651c";
const FREE_FLOOR: u64 = 3_489_660_928;
const ARTIFACT_CAP: u64 = 12 * 1024 * 1024;
const PIPE_CAP: usize = 512 * 1024;

fn private_file(path: &Path) -> Result<File> {
    Ok(OpenOptions::new().write(true).create_new(true).mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(path)?)
}
fn hash_file(path: &Path) -> Result<(String, u64)> {
    let mut file = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(path)?;
    let before = file.metadata()?;
    ensure!(before.is_file() && before.nlink() == 1 && before.len() <= ELF_BYTES, "fixture input is not a bounded single regular file");
    let mut hash = Sha256::new(); let mut buf = [0u8; 65536]; let mut bytes = 0;
    loop { let n = file.read(&mut buf)?; if n == 0 { break; } bytes += n as u64; ensure!(bytes <= before.len(), "fixture input grew during read"); hash.update(&buf[..n]); }
    let after = fs::symlink_metadata(path)?;
    ensure!(after.dev() == before.dev() && after.ino() == before.ino() && after.len() == bytes
        && !after.file_type().is_symlink(), "fixture input changed during read");
    Ok((format!("{:x}", hash.finalize()), bytes))
}
fn input(name: &str, directory: bool) -> Result<PathBuf> {
    let value = std::env::var(name).with_context(|| format!("explicit fixture DATA input {name} required"))?;
    let path = PathBuf::from(value); ensure!(path.is_absolute() && path.canonicalize()? == path, "fixture input must be canonical");
    let metadata = fs::symlink_metadata(&path)?;
    ensure!(!metadata.file_type().is_symlink() && if directory { metadata.is_dir() } else { metadata.is_file() }, "fixture input type differs");
    Ok(path)
}
fn disk_bytes(path: &Path) -> Result<u64> {
    let mut bytes = 0u64;
    for entry in fs::read_dir(path)? { let entry = entry?; let metadata = fs::symlink_metadata(entry.path())?;
        bytes = bytes.checked_add(metadata.len()).context("fixture size overflow")?;
        if metadata.is_dir() { bytes = bytes.checked_add(disk_bytes(&entry.path())?).context("fixture size overflow")?; }
    } Ok(bytes)
}
fn free_bytes(path: &Path) -> Result<u64> {
    use std::os::unix::ffi::OsStrExt;
    let name = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    let mut value = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    ensure!(unsafe { libc::statvfs(name.as_ptr(), value.as_mut_ptr()) } == 0, "statvfs failed");
    let value = unsafe { value.assume_init() };
    (value.f_bavail as u64).checked_mul(value.f_frsize as u64).context("free size overflow")
}
fn remaining(end: Instant) -> Result<Duration> {
    let value = end.saturating_duration_since(Instant::now()); ensure!(!value.is_zero(), "original fixture deadline expired"); Ok(value)
}

#[derive(Default)]
struct PipeData { bytes: usize, eof: bool, error: Option<String>, sha256: String, frames: Vec<Value> }
struct PipeOwner { data: Arc<Mutex<PipeData>>, worker: OwnedWorkerJoin, joined: Option<WorkerJoined> }
impl PipeOwner {
    fn spawn<R: Read + Send + 'static>(mut pipe: R, path: PathBuf, structured: bool) -> Result<Self> {
        let mut output = private_file(&path)?;
        let data = Arc::new(Mutex::new(PipeData::default())); let observed = Arc::clone(&data);
        let worker = std::thread::Builder::new().name("codex-fixture-pipe".into()).spawn(move || {
            let result = (|| -> Result<()> {
                let mut hash = Sha256::new(); let mut pending = Vec::new(); let mut buf = [0u8; 8192]; let mut bytes = 0;
                loop { let n = pipe.read(&mut buf)?;
                    if n == 0 { let mut state = observed.lock().unwrap(); ensure!(pending.is_empty(), "partial provider frame at EOF");
                        state.eof = true; state.sha256 = format!("{:x}", hash.finalize()); break; }
                    bytes += n; ensure!(bytes <= PIPE_CAP, "fixture pipe bound");
                    ensure!(free_bytes(path.parent().unwrap())? > FREE_FLOOR + 1024 * 1024, "fixture disk floor");
                    output.write_all(&buf[..n])?; hash.update(&buf[..n]);
                    if structured { pending.extend_from_slice(&buf[..n]); ensure!(pending.len() <= 65536, "provider frame bound");
                        while let Some(at) = pending.iter().position(|b| *b == b'\n') {
                            let line: Vec<_> = pending.drain(..=at).collect();
                            let frame: Value = serde_json::from_slice(&line)?;
                            let mut state = observed.lock().unwrap(); ensure!(state.frames.len() < 1024, "provider frame count"); state.frames.push(frame);
                        }
                    }
                    observed.lock().unwrap().bytes = bytes;
                } output.sync_data()?; Ok(())
            })();
            if let Err(error) = result { observed.lock().unwrap_or_else(|p| p.into_inner()).error = Some(format!("{error:#}")); }
        })?;
        Ok(Self { data, worker: OwnedWorkerJoin::new(worker), joined: None })
    }
    fn settle(&mut self, end: Instant) -> Result<()> {
        self.joined = Some(self.worker.await_joined(remaining(end)?)?);
        let state = self.data.lock().unwrap(); ensure!(state.eof && state.error.is_none(), "actual pipe EOF absent/error: {:?}", state.error); Ok(())
    }
    fn frames(&self) -> Result<Vec<Value>> {
        let state = self.data.lock().unwrap(); ensure!(state.error.is_none(), "provider reader failed"); Ok(state.frames.clone())
    }
    fn receipt(&self) -> Value { let state = self.data.lock().unwrap_or_else(|p|p.into_inner()); json!({"bytes":state.bytes,"eof":state.eof,"error":state.error,"sha256":state.sha256,"actual_join":self.joined.is_some()}) }
}

/// Retains the actual successful spawn BEFORE fallible pipe preparation.
struct RetainedChild { child: Child, stdout: Option<PipeOwner>, stderr: Option<PipeOwner>, exit: Option<ExitStatus>, term_requested: bool }
impl RetainedChild {
    fn spawn_into(slot: &mut Option<Self>, command: &mut Command, root: &Path, label: &str, structured: bool) -> Result<()> {
        ensure!(slot.is_none(), "child owner slot already occupied");
        command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let parent = unsafe { libc::getpid() };
        unsafe { command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 { return Err(std::io::Error::last_os_error()); }
            if libc::getppid() != parent { return Err(std::io::Error::other("fixture parent replaced")); }
            let limit = libc::rlimit { rlim_cur: 4 * 1024 * 1024, rlim_max: 4 * 1024 * 1024 };
            if libc::setrlimit(libc::RLIMIT_FSIZE, &limit) != 0 { return Err(std::io::Error::last_os_error()); } Ok(())
        }); }
        *slot = Some(Self { child: command.spawn()?, stdout: None, stderr: None, exit: None, term_requested: false });
        let held = slot.as_mut().unwrap();
        let stdout = held.child.stdout.take().context("actual stdout absent")?;
        held.stdout = Some(PipeOwner::spawn(stdout, root.join(format!("{label}.stdout.private")), structured)?);
        let stderr = held.child.stderr.take().context("actual stderr absent")?;
        held.stderr = Some(PipeOwner::spawn(stderr, root.join(format!("{label}.stderr.private")), false)?); Ok(())
    }
    fn send(&mut self, frame: Value) -> Result<()> {
        let bytes = serde_json::to_vec(&frame)?; ensure!(bytes.len() <= 4096, "provider control bound");
        let stdin = self.child.stdin.as_mut().context("provider stdin closed")?; stdin.write_all(&bytes)?; stdin.write_all(b"\n")?; stdin.flush()?; Ok(())
    }
    fn request_stop(&mut self) -> Result<()> {
        self.child.stdin.take();
        if self.exit.is_none() { self.exit = self.child.try_wait()?; }
        if self.exit.is_none() && !self.term_requested {
            self.term_requested = true;
            let result = unsafe { libc::kill(self.child.id() as libc::pid_t, libc::SIGTERM) };
            if result != 0 { self.exit = self.child.try_wait()?; ensure!(self.exit.is_some(), "actual child SIGTERM failed"); }
        } Ok(())
    }
    fn settle(&mut self, end: Instant) -> Result<()> {
        let kill_after = Instant::now() + Duration::from_secs(1);
        while self.exit.is_none() { remaining(end)?; self.exit = self.child.try_wait()?;
            if self.exit.is_none() && Instant::now() >= kill_after { self.child.kill()?; }
            if self.exit.is_none() { std::thread::sleep(Duration::from_millis(2)); }
        }
        self.stdout.as_mut().context("stdout owner absent")?.settle(end)?;
        self.stderr.as_mut().context("stderr owner absent")?.settle(end)?; Ok(())
    }
    fn receipt(&self) -> Value { json!({"actual_spawn_pid":self.child.id(),"reaped":self.exit.is_some(),"exit":self.exit.as_ref().map(|s|format!("{s}")),
        "stdout":self.stdout.as_ref().map(PipeOwner::receipt),"stderr":self.stderr.as_ref().map(PipeOwner::receipt)}) }
}

#[derive(Default)]
struct TapState { frames: Vec<Value>, bytes: usize, interrupt_id: Option<Value>, ack: bool, held: Option<tungstenite::Message>,
    held_value: Option<Value>, release: bool, forwarded: bool, error: Option<String> }
struct Tap { stopped: Arc<AtomicBool>, state: Arc<Mutex<TapState>>, worker: Option<OwnedWorkerJoin>, joined: Option<WorkerJoined> }
impl Tap {
    fn start_into(slot: &mut Option<Self>, path: &Path, real: PathBuf, thread: String, turn: String, end: Instant) -> Result<()> {
        ensure!(slot.is_none(), "tap slot occupied");
        *slot = Some(Self { stopped: Arc::new(AtomicBool::new(false)), state: Arc::new(Mutex::new(TapState::default())), worker: None, joined: None });
        let tap = slot.as_mut().unwrap(); let listener = std::os::unix::net::UnixListener::bind(path)?; listener.set_nonblocking(true)?;
        let stopped = Arc::clone(&tap.stopped); let state = Arc::clone(&tap.state);
        let worker = std::thread::Builder::new().name("codex-fixture-wire-tap".into()).spawn(move || {
            let result = (|| -> Result<()> {
                let stream = loop { remaining(end)?; ensure!(!stopped.load(Ordering::Acquire), "tap stopped before accept");
                    match listener.accept() { Ok((stream,_)) => break stream, Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(2)), Err(e) => return Err(e.into()) }
                };
                stream.set_read_timeout(Some(Duration::from_secs(2)))?; stream.set_write_timeout(Some(Duration::from_secs(1)))?;
                let mut client = tungstenite::accept(stream).map_err(|_|anyhow::anyhow!("fixture websocket accept failed"))?; let mut backend = connect(&real, &stopped, None)?;
                client.get_ref().set_read_timeout(Some(Duration::from_millis(5)))?; backend.socket.get_ref().set_read_timeout(Some(Duration::from_millis(5)))?;
                while !stopped.load(Ordering::Acquire) { remaining(end)?;
                    { let mut s = state.lock().unwrap(); if s.release { if let Some(frame) = s.held.take() { client.send(frame)?; s.forwarded = true; } } }
                    for direction in ["adapter_to_codex", "codex_to_adapter"] {
                        let incoming = if direction == "adapter_to_codex" { client.read() } else { backend.socket.read() };
                        let frame = match incoming { Ok(tungstenite::Message::Close(_)) => return Ok(()), Ok(frame) => frame,
                            Err(tungstenite::Error::Io(e)) if matches!(e.kind(), std::io::ErrorKind::WouldBlock|std::io::ErrorKind::TimedOut) => continue,
                            Err(tungstenite::Error::ConnectionClosed|tungstenite::Error::AlreadyClosed) => return Ok(()), Err(e) => return Err(e.into()) };
                        let value = if let tungstenite::Message::Text(text) = &frame { Some(serde_json::from_str::<Value>(text)?) } else { None };
                        let mut hold = false; let mut is_ack = false;
                        if let Some(value) = value { let mut s = state.lock().unwrap(); s.bytes += serde_json::to_vec(&value)?.len();
                            ensure!(s.bytes <= 1024 * 1024 && s.frames.len() < 1024, "tap capture bound");
                            if direction == "adapter_to_codex" && value["method"] == "turn/interrupt" { s.interrupt_id = Some(value["id"].clone()); }
                            if direction == "codex_to_adapter" && s.interrupt_id.as_ref() == value.get("id") && value.get("result") == Some(&json!({})) { is_ack = true; }
                            if direction == "codex_to_adapter" && value["method"] == "turn/completed" && value["params"]["threadId"] == thread
                                && value["params"]["turn"]["id"] == turn && value["params"]["turn"]["status"] == "interrupted" {
                                ensure!(s.held_value.is_none(), "duplicate original terminal notification"); s.held = Some(frame.clone()); s.held_value = Some(value.clone()); hold = true;
                            }
                            s.frames.push(json!({"direction":direction,"frame":value}));
                        }
                        if !hold { if direction == "adapter_to_codex" { backend.socket.send(frame)?; } else { client.send(frame)?; } if is_ack { state.lock().unwrap().ack = true; } }
                    }
                } Ok(())
            })();
            if let Err(error) = result { state.lock().unwrap_or_else(|p| p.into_inner()).error = Some(format!("{error:#}")); }
        })?;
        tap.worker = Some(OwnedWorkerJoin::new(worker)); Ok(())
    }
}

struct Owner { artifacts: PathBuf, sockets: PathBuf, run_end: Instant, cleanup_end: Instant,
    codex: Option<RetainedChild>, provider: Option<RetainedChild>, frontend: Option<Rpc>, frontend_frames: Vec<Value>, frontend_bytes: usize,
    tap: Option<Tap>, actor: Option<AgentLinkHandle>, actor_join: Option<AgentLinkWorkerJoined>, events: Vec<AgentLinkEvent> }
impl Owner {
    fn bounds(&self) -> Result<()> { remaining(self.run_end)?; ensure!(disk_bytes(&self.artifacts)? <= ARTIFACT_CAP && free_bytes(&self.artifacts)? > FREE_FLOOR + 1024 * 1024, "fixture disk bound"); Ok(()) }
    fn provider_frames(&self) -> Result<Vec<Value>> { self.provider.as_ref().context("provider owner absent")?.stdout.as_ref().context("provider pipe absent")?.frames() }
    fn provider_until(&self, predicate: impl Fn(&Value)->bool) -> Result<Value> {
        loop { self.bounds()?; let frames = self.provider_frames()?; ensure!(!frames.iter().any(|v| v["kind"] == "failure"), "provider failed");
            if let Some(frame) = frames.into_iter().find(&predicate) { return Ok(frame); } std::thread::sleep(Duration::from_millis(2)); }
    }
    fn frontend_next(&mut self) -> Result<Option<Value>> {
        self.bounds()?; let frame = self.frontend.as_mut().context("frontend absent")?.read()?;
        if let Some(frame) = &frame { self.frontend_bytes+=serde_json::to_vec(frame)?.len();ensure!(self.frontend_bytes<=1024*1024&&self.frontend_frames.len()<1024, "frontend frame bound"); self.frontend_frames.push(frame.clone()); } Ok(frame)
    }
    fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        self.bounds()?; let id = self.frontend.as_mut().context("frontend absent")?.request(method,params)?;
        loop { if let Some(frame) = self.frontend_next()? { if frame.get("method").is_none() && frame["id"].as_u64()==Some(id) {
            ensure!(frame.get("error").is_none(), "actual Codex RPC rejected: {frame}"); return frame.get("result").cloned().context("RPC result absent"); } } }
    }
    fn frontend_until(&mut self, predicate: impl Fn(&Value)->bool) -> Result<Value> {
        loop { if let Some(frame) = self.frontend_frames.iter().find(|v| predicate(v)) { return Ok(frame.clone()); } self.frontend_next()?; }
    }
    fn actor_until(&mut self, events: &Receiver<AgentLinkEvent>, predicate: impl Fn(&AgentLinkEventKind)->bool, run: &str, attempt: &str) -> Result<AgentLinkEvent> {
        loop { self.bounds()?; match events.recv_timeout(Duration::from_millis(20)) {
            Ok(event) => { ensure!(event.run_id==run && event.attempt_id==attempt,"foreign adapter event"); let matched=predicate(&event.kind);
                ensure!(!matches!(&event.kind,AgentLinkEventKind::Closed{..}),"adapter closed before qualification"); ensure!(matched||!matches!(&event.kind,AgentLinkEventKind::CommandResult{..}),"unexpected actual adapter result: {event:?}"); self.events.push(event.clone()); if matched { return Ok(event); } }
            Err(mpsc::RecvTimeoutError::Timeout)=>{},Err(e)=>return Err(e.into()) }
        }
    }
    fn settle(&mut self) -> Result<()> {
        let cleanup_end=self.cleanup_end;let mut errors=Vec::new(); self.frontend.take();
        if let Some(actor)=&self.actor { actor.close(); } if let Some(tap)=&self.tap { tap.stopped.store(true,Ordering::Release); }
        if let Some(provider)=&mut self.provider { let _=provider.send(json!({"op":"stop"})); provider.child.stdin.take(); }
        if let Some(codex)=&mut self.codex { if let Err(e)=codex.request_stop(){errors.push(e.to_string());} }
        if let Some(actor)=&mut self.actor { match remaining(cleanup_end).and_then(|d|actor.await_closed(d)) { Ok(joined)=>self.actor_join=Some(joined),Err(e)=>errors.push(e.to_string()) } }
        if let Some(tap)=&mut self.tap { match tap.worker.as_ref().context("tap worker absent").and_then(|w|w.await_joined(remaining(cleanup_end)?)) { Ok(joined)=>tap.joined=Some(joined),Err(e)=>errors.push(e.to_string()) } }
        for child in [&mut self.codex,&mut self.provider].into_iter().flatten() {
            if let Err(e)=child.request_stop().and_then(|_|child.settle(cleanup_end)){errors.push(e.to_string());}
        }
        ensure!(errors.is_empty(),"actual fixture custody remains retained: {}",errors.join("; ")); Ok(())
    }
    fn receipt(&self) -> Value { let tap=self.tap.as_ref().map(|t| {let s=t.state.lock().unwrap_or_else(|p|p.into_inner());json!({"frames":s.frames,"real_interrupt_ack":s.ack,"original_terminal":s.held_value,"original_terminal_forwarded":s.forwarded,"error":s.error,"actual_join":t.joined.is_some()})});
        json!({"codex":self.codex.as_ref().map(RetainedChild::receipt),"provider":self.provider.as_ref().map(RetainedChild::receipt),"actor_actually_joined":self.actor_join.is_some(),"tap":tap,"frontend_frames":self.frontend_frames,"adapter_events":self.events}) }
}

fn execute(owner: &mut Owner, binary: &Path, node: &Path, script: &Path, nonce: &str) -> Result<()> {
    let home=owner.artifacts.join("home");let project=owner.artifacts.join("project");let tmp=owner.artifacts.join("tmp");
    for dir in [&home,&project,&tmp] {fs::create_dir(dir)?;fs::set_permissions(dir,fs::Permissions::from_mode(0o700))?;} fs::create_dir(home.join(".codex"))?;
    let mut provider=Command::new(node);provider.arg(script).args(["--serve",nonce]).current_dir(&project).env_clear().env("HOME",&home).env("PATH","/usr/bin:/bin");
    RetainedChild::spawn_into(&mut owner.provider,&mut provider,&owner.artifacts,"provider",true)?;
    let ready=owner.provider_until(|v|v["kind"]=="ready")?;let origin=ready["baseUrl"].as_str().context("provider endpoint absent")?.to_string();
    ensure!(origin.starts_with("http://127.0.0.1:") && origin[17..].parse::<u16>().is_ok(),"provider endpoint differs");
    ensure!(ready["node"].as_str().is_some_and(|v|v.starts_with("v22.")),"fixture needs Node22");
    let real=owner.sockets.join("real.sock");let proxy=owner.sockets.join("cx.sock");
    let configs=[("model",json!("fixture-model")),("model_provider",json!("rudder_fixture")),("notify",json!([])),("cli_auth_credentials_store",json!("ephemeral")),
        ("check_for_update_on_startup",json!(false)),("analytics.enabled",json!(false)),("feedback.enabled",json!(false)),("web_search",json!("disabled")),("features.plugins",json!(false)),("features.computer_use",json!(false)),
        ("model_providers.rudder_fixture.name",json!("Rudder synthetic loopback fixture")),("model_providers.rudder_fixture.base_url",json!(format!("{origin}/v1"))),
        ("model_providers.rudder_fixture.env_key",json!("RUDDER_FIXTURE_KEY")),("model_providers.rudder_fixture.wire_api",json!("responses")),("model_providers.rudder_fixture.requires_openai_auth",json!(false)),
        ("model_providers.rudder_fixture.supports_websockets",json!(false)),("model_providers.rudder_fixture.request_max_retries",json!(0)),("model_providers.rudder_fixture.stream_max_retries",json!(0))];
    let mut command=Command::new(binary);command.current_dir(&project).env_clear().env("HOME",&home).env("CODEX_HOME",home.join(".codex")).env("TMPDIR",&tmp).env("PATH","/usr/bin:/bin").env("LANG","C.UTF-8").env("TERM","dumb")
        .env("RUDDER_FIXTURE_KEY","synthetic-loopback-fixture-not-a-credential").env("HTTP_PROXY",&origin).env("HTTPS_PROXY",&origin).env("ALL_PROXY",&origin).env("NO_PROXY","127.0.0.1,localhost");
    for (key,value) in configs {command.args(["-c",&format!("{key}={value}")]);}command.args(["app-server","--listen",&format!("unix://{}",real.display())]);
    RetainedChild::spawn_into(&mut owner.codex,&mut command,&owner.artifacts,"codex",false)?;
    owner.frontend=Some(connect(&real,&AtomicBool::new(false),None)?);
    let initialized=owner.call("initialize",json!({"clientInfo":{"name":"rudder_fixture_frontend","version":"1"},"capabilities":{"experimentalApi":true,"requestAttestation":false}}))?;
    ensure!(initialized["userAgent"].as_str().is_some_and(|v|v.contains("/0.166.0 ")),"actual Codex version differs");
    owner.frontend.as_mut().unwrap().send(&json!({"method":"initialized"}))?;
    let thread=owner.call("thread/start",json!({"model":"fixture-model","modelProvider":"rudder_fixture","cwd":project,"approvalPolicy":"never","sandbox":"danger-full-access","ephemeral":false}))?;
    let thread=thread.pointer("/thread/id").and_then(Value::as_str).context("actual thread absent")?.to_string();
    let first=owner.call("turn/start",json!({"threadId":thread,"input":[{"type":"text","text":"Perform the synthetic fixture tool edit."}]}))?;
    let first=first.pointer("/turn/id").and_then(Value::as_str).context("first turn absent")?.to_string();
    owner.frontend_until(|v|v["method"]=="turn/completed"&&v["params"]["threadId"]==thread&&v["params"]["turn"]["id"]==first&&v["params"]["turn"]["status"]=="completed")?;
    ensure!(owner.frontend_frames.iter().any(|v|v["method"]=="item/completed"&&v["params"]["threadId"]==thread&&v["params"]["turnId"]==first&&v["params"]["item"]["id"]=="call_fixture"&&v["params"]["item"]["exitCode"]==0),"actual successful tool completion absent");
    ensure!(hash_file(&project.join("fixture.txt"))? == ("5293751da30620a50e6bde3cef86855f8049fd0ecade6ffe5ce5b2e6deb5a658".to_string(), 23), "actual tool file differs");
    let second=owner.call("turn/start",json!({"threadId":thread,"input":[{"type":"text","text":"Hold the synthetic provider open for native controls."}]}))?;
    let second=second.pointer("/turn/id").and_then(Value::as_str).context("control turn absent")?.to_string();owner.provider_until(|v|v["kind"]=="held"&&v["index"]==2)?;
    Tap::start_into(&mut owner.tap,&proxy,real,thread.clone(),second.clone(),owner.cleanup_end)?;
    let run=format!("fixture-{nonce}");let attempt=uuid::Uuid::new_v4().to_string();let (tx,rx)=mpsc::channel();
    owner.actor=Some(spawn_link(AgentLinkLaunch{format_version:1,run_id:run.clone(),attempt_id:attempt.clone(),backend:"codex".into(),socket_path:proxy,log_path:owner.artifacts.join("adapter.log")},&project,Some(thread.clone()),tx)?);
    owner.actor_until(&rx,|event|matches!(event,AgentLinkEventKind::Bound{thread_id,active_turn_id:Some(turn),..} if thread_id==&thread&&turn==&second),&run,&attempt)?;
    let marker=format!("RUDDER_NATIVE_FIXTURE_STEER_{nonce}");let message=PeerMessage{sender_run_id:"synthetic-peer".into(),message_id:uuid::Uuid::new_v4().to_string(),text:marker.clone()};
    let make=|id:&str,operation:AgentLinkOperation,turn:Option<String>|AgentLinkCommand{command_id:id.into(),run_id:run.clone(),attempt_id:attempt.clone(),expected_thread_id:thread.clone(),expected_turn_id:turn,operation,timeout:Duration::from_secs(5)};
    owner.actor.as_ref().unwrap().submit(make("wrong-turn",AgentLinkOperation::Steer{message:message.clone()},Some("wrong-turn".into())))?;
    owner.actor_until(&rx,|e|matches!(e,AgentLinkEventKind::CommandResult{command_id,outcome:AgentLinkOutcome::Rejected{..}} if command_id=="wrong-turn"),&run,&attempt)?;
    owner.actor.as_ref().unwrap().submit(make("steer",AgentLinkOperation::Steer{message:message.clone()},Some(second.clone())))?;
    owner.actor_until(&rx,|e|matches!(e,AgentLinkEventKind::CommandResult{command_id,outcome:AgentLinkOutcome::TransportAccepted{turn_id}} if command_id=="steer"&&turn_id==&second),&run,&attempt)?;
    {let tap=owner.tap.as_ref().unwrap().state.lock().unwrap();let frame=tap.frames.iter().find(|v|v["direction"]=="adapter_to_codex"&&v["frame"]["method"]=="turn/steer").context("actual native steer absent")?;
        ensure!(frame["frame"]["params"]["expectedTurnId"]==second,"native steer turn differs");let context=frame["frame"]["params"]["additionalContext"].as_object().context("typed peer context absent")?;
        ensure!(context.len()==1,"peer context count differs");let value=context.values().next().unwrap();ensure!(value["kind"]=="untrusted"&&value["name"]=="rudder_peer","peer became trusted/human input");
        let actual:PeerMessage=serde_json::from_str(value["value"].as_str().context("peer value absent")?)?;ensure!(actual.sender_run_id==message.sender_run_id&&actual.message_id==message.message_id&&actual.text==message.text,"peer identity/text differs");}
    owner.provider.as_mut().unwrap().send(json!({"op":"finishHeld","index":2}))?;
    owner.provider_until(|v|v["kind"]=="request"&&v["index"]==3&&v["sawSteer"]==true)?;
    owner.actor.as_ref().unwrap().submit(make("interrupt",AgentLinkOperation::Interrupt,Some(second.clone())))?;
    loop{owner.bounds()?;let tap=owner.tap.as_ref().unwrap().state.lock().unwrap();ensure!(tap.error.is_none(),"tap failed");if tap.ack&&tap.held_value.is_some(){break;}drop(tap);std::thread::sleep(Duration::from_millis(2));}
    // Actual {} ACK was forwarded. Hold only the original actual interrupted
    // notification: the production actor MUST NOT confirm stop from ACK alone.
    let hold_until=Instant::now()+Duration::from_millis(120);
    while Instant::now()<hold_until {owner.bounds()?;match rx.recv_timeout(Duration::from_millis(10)){Ok(event)=>{ensure!(event.run_id==run&&event.attempt_id==attempt,"foreign adapter hold event");ensure!(!matches!(&event.kind,AgentLinkEventKind::CommandResult{command_id,outcome:AgentLinkOutcome::InterruptConfirmed{..}} if command_id=="interrupt"),"ACK alone confirmed interruption");owner.events.push(event);},Err(mpsc::RecvTimeoutError::Timeout)=>{},Err(e)=>return Err(e.into())}}
    owner.provider_until(|v|v["kind"]=="responseClosed"&&v["index"]==3&&v["fixtureEnded"]==false)?;
    owner.tap.as_ref().unwrap().state.lock().unwrap().release=true;
    owner.actor_until(&rx,|e|matches!(e,AgentLinkEventKind::CommandResult{command_id,outcome:AgentLinkOutcome::InterruptConfirmed{turn_id}} if command_id=="interrupt"&&turn_id==&second),&run,&attempt)?;
    owner.frontend_until(|v|v["method"]=="turn/completed"&&v["params"]["threadId"]==thread&&v["params"]["turn"]["id"]==second&&v["params"]["turn"]["status"]=="interrupted")?;
    owner.actor.as_ref().unwrap().submit(make("stale-steer",AgentLinkOperation::Steer{message},Some(second)))?;
    owner.actor_until(&rx,|e|matches!(e,AgentLinkEventKind::CommandResult{command_id,outcome:AgentLinkOutcome::Rejected{..}} if command_id=="stale-steer"),&run,&attempt)?;
    ensure!(owner.provider_frames()?.iter().filter(|v|v["kind"]=="request").count()==4,"adapter replayed/created substitute request");
    {let tap=owner.tap.as_ref().unwrap().state.lock().unwrap();let sent:Vec<_>=tap.frames.iter().filter(|v|v["direction"]=="adapter_to_codex").collect();
        ensure!(sent.iter().filter(|v|v["frame"]["method"]=="turn/steer").count()==1&&sent.iter().filter(|v|v["frame"]["method"]=="turn/interrupt").count()==1,"wrong/stale controls reached backend");
        ensure!(!sent.iter().any(|v|matches!(v["frame"]["method"].as_str(),Some("thread/start"|"thread/fork"|"turn/start"))),"adapter created substitute conversation/turn");}Ok(())
}

#[test]
#[ignore = "actual pinned Codex and Node DATA inputs; synthetic loopback only; Root runs after compilation"]
fn actual_codex_loopback_adapter_controls() {
    let result = (|| -> Result<()> {
        let binary = input("RUDDER_REAL_CODEX_BIN", false)?;
        let node = input("RUDDER_CODEX_PROTOCOL_NODE", false)?;
        let script = input("RUDDER_CODEX_PROTOCOL_PROVIDER", false)?;
        let provenance = input("RUDDER_CODEX_PROTOCOL_PROVENANCE", false)?;
        let root = input("RUDDER_CODEX_PROTOCOL_ARTIFACT_ROOT", true)?;
        let metadata = fs::symlink_metadata(&root)?;
        ensure!(metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o777 == 0o700, "artifact root not private");
        ensure!(hash_file(&binary)? == (ELF_SHA.to_string(), ELF_BYTES), "actual CLI differs from sealed official ELF");
        ensure!(hash_file(&script)?.0 == PROVIDER_SHA, "provider script differs from reviewed fixture");
        ensure!(fs::metadata(&provenance)?.len() <= 65536, "provenance DATA bound");
        let source: Value = serde_json::from_slice(&fs::read(&provenance)?)?;
        ensure!(source["archiveIntegrityVerified"] == true && source["selected"]["sha256"] == ELF_SHA && source["selected"]["bytes"] == ELF_BYTES,
            "provenance DATA differs; it cannot replace actual ELF verification");
        ensure!(free_bytes(&root)? > FREE_FLOOR + 16 * 1024 * 1024, "fixture disk floor/headroom");
        let start = Instant::now();
        let nonce = uuid::Uuid::new_v4().to_string();
        let artifacts = root.join(format!("native-codex-{nonce}"));
        let sockets = PathBuf::from("/tmp").join(format!("rudder-cx-{}", uuid::Uuid::new_v4().simple()));
        for dir in [&artifacts, &sockets] {
            fs::create_dir(dir)?;
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        }
        let mut owner = Box::new(Owner {
            artifacts, sockets, run_end: start + Duration::from_secs(55), cleanup_end: start + Duration::from_secs(75),
            codex: None, provider: None, frontend: None, frontend_frames: Vec::new(), frontend_bytes: 0,
            tap: None, actor: None, actor_join: None, events: Vec::new(),
        });
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| execute(&mut owner, &binary, &node, &script, &nonce)));
        let cleanup = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| owner.settle()))
            .unwrap_or_else(|_| Err(anyhow::anyhow!("fixture cleanup panicked; actual owner retained")));
        let final_inputs = (|| -> Result<()> {
            ensure!(hash_file(&binary)? == (ELF_SHA.to_string(), ELF_BYTES), "CLI changed during test");
            ensure!(hash_file(&script)?.0 == PROVIDER_SHA, "provider changed during test");
            Ok(())
        })();
        let passed = matches!(&outcome, Ok(Ok(()))) && cleanup.is_ok() && final_inputs.is_ok();
        let record = json!({
            "format_version":1, "scope":"actual-rudder-native-adapter-synthetic-loopback-only", "qualified":passed,
            "paid_model_calls":0, "production_startup":false, "spec3":false, "managed_backend_witness":false,
            "model_qualification":false, "network_isolation":false, "elf_sha256":ELF_SHA, "provider_sha256":PROVIDER_SHA,
            "tool_file":{"path":"project/fixture.txt", "expected_bytes":23,
                "expected_sha256":"5293751da30620a50e6bde3cef86855f8049fd0ecade6ffe5ce5b2e6deb5a658",
                "validated_before_controls":matches!(&outcome, Ok(Ok(())))},
            "actors_and_processes":owner.receipt(), "run_ok":matches!(&outcome, Ok(Ok(()))),
            "cleanup_ok":cleanup.is_ok(), "final_inputs_ok":final_inputs.is_ok(),
        });
        // Keep the actual owner outside this fallible receipt-write scope too.
        let report_written = (|| -> Result<()> {
            let bytes = serde_json::to_vec_pretty(&record)?;
            ensure!(disk_bytes(&owner.artifacts)?.checked_add(bytes.len() as u64).is_some_and(|n| n <= ARTIFACT_CAP), "final artifact cap");
            ensure!(free_bytes(&owner.artifacts)? > FREE_FLOOR + bytes.len() as u64, "final disk floor");
            let mut file = private_file(&owner.artifacts.join("report.json"))?;
            file.write_all(&bytes)?;
            file.sync_data()?;
            Ok(())
        })();
        if cleanup.is_err() {
            let location = owner.artifacts.clone();
            let _retained = Box::leak(owner);
            bail!("fixture cleanup uncertain; SAME actual carrier retained at {}", location.display());
        }
        report_written.context("write bounded private fixture receipt")?;
        // No fixture directories are removed; Root retains every captured fact.
        match outcome {
            Ok(result) => result?,
            Err(_) => bail!("native fixture panicked; actual resources settled before reporting failure"),
        }
        final_inputs?;
        println!("{}", json!({"scope":"actual-rudder-native-adapter-synthetic-loopback-only", "qualified":true,
            "report":owner.artifacts.join("report.json"), "paid_model_calls":0, "spec3":false}));
        Ok(())
    })();
    if let Err(error) = result { panic!("{error:#}"); }
}
