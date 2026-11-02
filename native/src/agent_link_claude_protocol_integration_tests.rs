//! Ignored actual production Rust Claude adapter, official retained CLI and
//! production protocol class over a qualification-only local RPC relay.
//! No managed witness, production launcher/renderer, profiles or paid models.
use super::*;
use std::{fs::File,io::Read,os::{fd::{AsRawFd,FromRawFd,OwnedFd},unix::process::CommandExt},process::{Child,ChildStdin,Command,ExitStatus,Stdio},sync::Mutex};
const ELF_SHA:&str="fd13c70000fec3f2116dfbcbb6121210b4a29328bf0e46210a59dd594a67d824";
const ELF_BYTES:u64=328_262_000;
const PROVIDER_SHA:&str="d5ea7107df2d2d601543955cd5795892d9f7dc0dcef81dec648a44eb438e2647";
const BRIDGE_SHA:&str="c4bfb11ba08dda76d5e6a5d44ddf8236c2b084bbd8d06483b9820b5966083e56";
const FREE_FLOOR:u64=3_489_660_928;
const ARTIFACT_CAP:u64=16*1024*1024;
const PIPE_CAP:usize=2*1024*1024;
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

type Input=Arc<Mutex<Option<ChildStdin>>>;
fn send(input:&Input,value:&Value,end:Instant)->Result<()> {
    let mut bytes=serde_json::to_vec(value)?;ensure!(bytes.len()<=1024*1024,"fixture input frame bound");bytes.push(b'\n');
    let mut held=loop {remaining(end)?;match input.try_lock(){Ok(held)=>break held,Err(std::sync::TryLockError::WouldBlock)=>std::thread::sleep(Duration::from_millis(1)),Err(_)=>bail!("original input poisoned")}};
    let fd=held.as_mut().context("original input closed")?.as_raw_fd();let mut at=0;
    while at<bytes.len(){remaining(end)?;let n=unsafe{libc::write(fd,bytes[at..].as_ptr().cast(),bytes.len()-at)};
        if n>0 {at+=n as usize;}else {let e=std::io::Error::last_os_error();if matches!(e.kind(),std::io::ErrorKind::WouldBlock|std::io::ErrorKind::Interrupted){std::thread::sleep(Duration::from_millis(1));}else{return Err(e.into());}}}Ok(())
}
fn close_input(input:&Input,end:Instant)->Result<()> {loop{remaining(end)?;match input.try_lock(){Ok(mut held)=>{held.take();return Ok(());},Err(std::sync::TryLockError::WouldBlock)=>std::thread::sleep(Duration::from_millis(1)),Err(_)=>bail!("original input poisoned")}}}
#[derive(Default)]struct PipeData {bytes:usize,eof:bool,error:Option<String>,sha256:String,frames:Vec<Value>}
type Forward=Arc<dyn Fn(&Value)->Result<()>+Send+Sync>;
struct PipeOwner {data:Arc<Mutex<PipeData>>,raw:Arc<Mutex<Option<Box<dyn Read+Send>>>>,worker:Option<OwnedWorkerJoin>,joined:Option<WorkerJoined>}
impl PipeOwner {
    fn start_retaining<R:Read+Send+'static>(out:&mut Option<Self>,pipe:R,path:PathBuf,structured:bool,forward:Option<Forward>)->Result<()> {
        ensure!(out.is_none(),"pipe output occupied");
        *out=Some(Self {data:Arc::new(Mutex::new(PipeData::default())),raw:Arc::new(Mutex::new(Some(Box::new(pipe)))),worker:None,joined:None});
        let held=out.as_mut().unwrap();let data=Arc::clone(&held.data);let raw=Arc::clone(&held.raw);let mut output=private_file(&path)?;
        let stored=Arc::new(AtomicBool::new(false));let worker_stored=Arc::clone(&stored);
        let worker=std::thread::Builder::new().name("claude-fixture-pipe".into()).spawn(move||{
            while !worker_stored.load(Ordering::Acquire){std::thread::yield_now();}
            let result=(||->Result<()>{let mut pipe=raw.lock().map_err(|_|anyhow::anyhow!("pipe owner poisoned"))?.take().context("actual pipe absent")?;
                let mut hash=Sha256::new();let mut pending=Vec::new();let mut total=0;let mut buf=[0u8;8192];
                loop{let n=pipe.read(&mut buf)?;if n==0{ensure!(pending.is_empty(),"partial JSON at actual EOF");let mut s=data.lock().unwrap();s.eof=true;s.sha256=format!("{:x}",hash.finalize());break;}
                    total+=n;ensure!(total<=PIPE_CAP,"actual pipe bound");ensure!(free_bytes(path.parent().unwrap())?>FREE_FLOOR+1024*1024,"fixture disk floor");output.write_all(&buf[..n])?;hash.update(&buf[..n]);
                    if structured {pending.extend_from_slice(&buf[..n]);ensure!(pending.len()<=1024*1024,"actual frame bound");
                        while let Some(at)=pending.iter().position(|b|*b==b'\n'){let line:Vec<_>=pending.drain(..=at).collect();if line.iter().all(u8::is_ascii_whitespace){continue;}let value:Value=serde_json::from_slice(&line)?;
                            {let mut s=data.lock().unwrap();ensure!(s.frames.len()<2048,"actual frame count");s.frames.push(value.clone());}if let Some(f)=&forward{f(&value)?;}}}
                    data.lock().unwrap().bytes=total;
                }output.sync_data()?;Ok(())})();if let Err(error)=result{data.lock().unwrap_or_else(|p|p.into_inner()).error=Some(format!("{error:#}"));}
        })?;
        held.worker=Some(OwnedWorkerJoin::new(worker));stored.store(true,Ordering::Release);Ok(())
    }
    fn settle(&mut self,end:Instant)->Result<()> {self.joined=Some(self.worker.as_ref().context("original pipe worker absent")?.await_joined(remaining(end)?)?);let s=self.data.lock().unwrap();ensure!(s.eof&&s.error.is_none(),"actual pipe EOF absent/error: {:?}",s.error);Ok(())}
    fn frames(&self)->Result<Vec<Value>>{let s=self.data.lock().unwrap();ensure!(s.error.is_none(),"original reader failed: {:?}",s.error);Ok(s.frames.clone())}
    fn receipt(&self)->Value{let s=self.data.lock().unwrap_or_else(|p|p.into_inner());json!({"bytes":s.bytes,"eof":s.eof,"error":s.error,"sha256":s.sha256,"actual_join":self.joined.is_some()})}
}
struct RetainedChild {child:Child,pidfd:Option<OwnedFd>,exe:Option<File>,input:Input,stdout:Option<PipeOwner>,stderr:Option<PipeOwner>,exit:Option<ExitStatus>,term_requested:bool}
impl RetainedChild {
    fn spawn_retaining(out:&mut Option<Self>,command:&mut Command,image:&Path)->Result<()> {
        ensure!(out.is_none(),"original child output occupied");command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());let parent=unsafe{libc::getpid()};
        unsafe{command.pre_exec(move||{if libc::prctl(libc::PR_SET_PDEATHSIG,libc::SIGKILL)!=0||libc::getppid()!=parent{return Err(std::io::Error::last_os_error());}
            let limit=libc::rlimit{rlim_cur:8*1024*1024,rlim_max:8*1024*1024};if libc::setrlimit(libc::RLIMIT_FSIZE,&limit)!=0{return Err(std::io::Error::last_os_error());}Ok(())});}
        *out=Some(Self{child:command.spawn()?,pidfd:None,exe:None,input:Arc::new(Mutex::new(None)),stdout:None,stderr:None,exit:None,term_requested:false});let held=out.as_mut().unwrap();
        // Original unreaped Child is retained before pidfd/proc-exe effects.
        let fd=unsafe{libc::syscall(libc::SYS_pidfd_open,held.child.id(),0) as i32};ensure!(fd>=0,"actual child pidfd unavailable");held.pidfd=Some(unsafe{OwnedFd::from_raw_fd(fd)});
        held.exe=Some(File::open(format!("/proc/{}/exe",held.child.id()))?);let actual=held.exe.as_ref().unwrap().metadata()?;let expected=fs::metadata(image)?;
        ensure!((actual.dev(),actual.ino(),actual.len())==(expected.dev(),expected.ino(),expected.len()),"original child image differs");
        let stdin=held.child.stdin.take().context("actual stdin absent")?;let flags=unsafe{libc::fcntl(stdin.as_raw_fd(),libc::F_GETFL)};ensure!(flags>=0&&unsafe{libc::fcntl(stdin.as_raw_fd(),libc::F_SETFL,flags|libc::O_NONBLOCK)}==0,"nonblocking actual input unavailable");*held.input.lock().unwrap()=Some(stdin);Ok(())
    }
    fn readers(&mut self,root:&Path,label:&str,structured:bool,forward:Option<Forward>)->Result<()> {
        PipeOwner::start_retaining(&mut self.stdout,self.child.stdout.take().context("actual stdout absent")?,root.join(format!("{label}.stdout.private")),structured,forward)?;
        PipeOwner::start_retaining(&mut self.stderr,self.child.stderr.take().context("actual stderr absent")?,root.join(format!("{label}.stderr.private")),false,None)
    }
    fn request_stop(&mut self,end:Instant)->Result<()> {close_input(&self.input,end)?;self.child.stdin.take();if self.exit.is_none(){self.exit=self.child.try_wait()?;}
        if self.exit.is_none()&&!self.term_requested{self.term_requested=true;let fd=self.pidfd.as_ref().context("original pidfd absent")?.as_raw_fd();let n=unsafe{libc::syscall(libc::SYS_pidfd_send_signal,fd,libc::SIGTERM,std::ptr::null::<libc::siginfo_t>(),0)};
            if n!=0{self.exit=self.child.try_wait()?;ensure!(self.exit.is_some(),"original child stop failed");}}Ok(())}
    fn settle(&mut self,end:Instant)->Result<()> {let kill_after=Instant::now()+Duration::from_secs(1);while self.exit.is_none(){remaining(end)?;self.exit=self.child.try_wait()?;if self.exit.is_none()&&Instant::now()>kill_after{let fd=self.pidfd.as_ref().context("original pidfd absent")?.as_raw_fd();ensure!(unsafe{libc::syscall(libc::SYS_pidfd_send_signal,fd,libc::SIGKILL,std::ptr::null::<libc::siginfo_t>(),0)}==0,"original child kill failed");}if self.exit.is_none(){std::thread::sleep(Duration::from_millis(2));}}
        let mut poll=libc::pollfd{fd:self.pidfd.as_ref().context("original pidfd absent")?.as_raw_fd(),events:libc::POLLIN,revents:0};ensure!(unsafe{libc::poll(&mut poll,1,0)}==1&&poll.revents&libc::POLLIN!=0,"original pidfd not exited");
        self.stdout.as_mut().context("actual stdout owner absent")?.settle(end)?;self.stderr.as_mut().context("actual stderr owner absent")?.settle(end)?;Ok(())}
    fn receipt(&self)->Value{json!({"actual_spawn_pid":self.child.id(),"pidfd_original":self.pidfd.is_some(),"exe_original":self.exe.is_some(),"reaped":self.exit.is_some(),"exit":self.exit.as_ref().map(|s|format!("{s}")),"stdout":self.stdout.as_ref().map(PipeOwner::receipt),"stderr":self.stderr.as_ref().map(PipeOwner::receipt)})}
}

#[derive(Default)]
struct TapState { frames: Vec<Value>, bytes: usize, interrupt_id: Option<Value>, ack: bool, held: Option<tungstenite::Message>,
    held_value: Option<Value>, hold_turn: Option<String>, release: bool, forwarded: bool, error: Option<String> }
struct Tap { stopped: Arc<AtomicBool>, state: Arc<Mutex<TapState>>, worker: Option<OwnedWorkerJoin>, joined: Option<WorkerJoined> }
impl Tap {
    fn start_into(slot: &mut Option<Self>, path: &Path, real: PathBuf, thread: String, end: Instant) -> Result<()> {
        ensure!(slot.is_none(), "tap slot occupied");
        *slot = Some(Self { stopped: Arc::new(AtomicBool::new(false)), state: Arc::new(Mutex::new(TapState::default())), worker: None, joined: None });
        let tap = slot.as_mut().unwrap(); let listener = std::os::unix::net::UnixListener::bind(path)?; listener.set_nonblocking(true)?;
        let stopped = Arc::clone(&tap.stopped); let state = Arc::clone(&tap.state);
        let worker = std::thread::Builder::new().name("claude-fixture-wire-tap".into()).spawn(move || {
            let result = (|| -> Result<()> {
                let stream = loop { remaining(end)?; ensure!(!stopped.load(Ordering::Acquire), "tap stopped before accept");
                    match listener.accept() { Ok((stream,_)) => break stream, Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(2)), Err(e) => return Err(e.into()) }
                };
                stream.set_read_timeout(Some(Duration::from_secs(2)))?; stream.set_write_timeout(Some(Duration::from_secs(1)))?;
                let mut client = tungstenite::accept(stream).map_err(|_|anyhow::anyhow!("fixture websocket accept failed"))?; let mut backend = connect(&real, &stopped, None)?;
                client.get_ref().set_read_timeout(Some(Duration::from_millis(5)))?; backend.socket.get_ref().set_read_timeout(Some(Duration::from_millis(5)))?;
                while !stopped.load(Ordering::Acquire) { remaining(end)?;
                    { let mut s = state.lock().unwrap(); if s.release { if let Some(frame) = s.held.take() { client.send(frame)?; s.forwarded = true; } } }
                    for direction in ["adapter_to_claude", "claude_to_adapter"] {
                        let incoming = if direction == "adapter_to_claude" { client.read() } else { backend.socket.read() };
                        let frame = match incoming { Ok(tungstenite::Message::Close(_)) => return Ok(()), Ok(frame) => frame,
                            Err(tungstenite::Error::Io(e)) if matches!(e.kind(), std::io::ErrorKind::WouldBlock|std::io::ErrorKind::TimedOut) => continue,
                            Err(tungstenite::Error::ConnectionClosed|tungstenite::Error::AlreadyClosed) => return Ok(()), Err(_) if stopped.load(Ordering::Acquire) => return Ok(()), Err(e) => return Err(e.into()) };
                        let value = if let tungstenite::Message::Text(text) = &frame { Some(serde_json::from_str::<Value>(text)?) } else { None };
                        let mut hold = false; let mut is_ack = false;
                        if let Some(value) = value { let mut s = state.lock().unwrap(); s.bytes += serde_json::to_vec(&value)?.len();
                            ensure!(s.bytes <= 1024 * 1024 && s.frames.len() < 1024, "tap capture bound");
                            if direction == "adapter_to_claude" && value["method"] == "turn/interrupt" { s.interrupt_id = Some(value["id"].clone()); }
                            if direction == "claude_to_adapter" && s.interrupt_id.as_ref() == value.get("id") && value.get("result") == Some(&json!({})) { is_ack = true; }
                            if direction == "claude_to_adapter" && value["method"] == "turn/completed" && value["params"]["threadId"] == thread
                                && s.hold_turn.as_deref() == value["params"]["turn"]["id"].as_str() && value["params"]["turn"]["status"] == "interrupted" {
                                ensure!(s.held_value.is_none(), "duplicate original terminal notification"); s.held = Some(frame.clone()); s.held_value = Some(value.clone()); hold = true;
                            }
                            s.frames.push(json!({"direction":direction,"frame":value}));
                        }
                        if !hold { if direction == "adapter_to_claude" { backend.socket.send(frame)?; } else { client.send(frame)?; } if is_ack { state.lock().unwrap().ack = true; } }
                    }
                } Ok(())
            })();
            if let Err(error) = result { state.lock().unwrap_or_else(|p| p.into_inner()).error = Some(format!("{error:#}")); }
        })?;
        tap.worker = Some(OwnedWorkerJoin::new(worker)); Ok(())
    }
}

#[derive(Default)]struct CliRelay {hold_terminal:bool,held:Option<Value>,released:bool,interrupts:Vec<String>,receipts:Vec<Value>,forwarded_receipts:Vec<String>,peer_inputs:Vec<Value>}
struct Owner {artifacts:PathBuf,sockets:PathBuf,run_end:Instant,cleanup_end:Instant,cli:Option<RetainedChild>,bridge:Option<RetainedChild>,provider:Option<RetainedChild>,
    stopping:Arc<AtomicBool>,relay:Arc<Mutex<CliRelay>>,tap:Option<Tap>,actor:Option<AgentLinkHandle>,actor_join:Option<AgentLinkWorkerJoined>,events:Vec<AgentLinkEvent>}
impl Owner {
    fn bounds(&self)->Result<()> {remaining(self.run_end)?;ensure!(disk_bytes(&self.artifacts)?<=ARTIFACT_CAP&&free_bytes(&self.artifacts)?>FREE_FLOOR+1024*1024,"fixture disk bound");Ok(())}
    fn frames(child:&Option<RetainedChild>)->Result<Vec<Value>> {child.as_ref().context("actual child absent")?.stdout.as_ref().context("actual stdout owner absent")?.frames()}
    fn provider_until(&self,predicate:impl Fn(&Value)->bool)->Result<Value>{loop{self.bounds()?;let frames=Self::frames(&self.provider)?;ensure!(!frames.iter().any(|v|v["kind"]=="failure"),"actual synthetic provider failed");if let Some(v)=frames.into_iter().find(&predicate){return Ok(v);}std::thread::sleep(Duration::from_millis(2));}}
    fn bridge_until(&self,predicate:impl Fn(&Value)->bool)->Result<Value>{loop{self.bounds()?;let frames=Self::frames(&self.bridge)?;ensure!(!frames.iter().any(|v|v["kind"]=="failure"||v["kind"]=="rpcFailure"),"protocol relay failed");if let Some(v)=frames.into_iter().find(&predicate){return Ok(v);}std::thread::sleep(Duration::from_millis(2));}}
    fn actor_until(&mut self,events:&Receiver<AgentLinkEvent>,predicate:impl Fn(&AgentLinkEventKind)->bool,run:&str,attempt:&str)->Result<AgentLinkEvent>{loop{self.bounds()?;match events.recv_timeout(Duration::from_millis(20)){Ok(event)=>{ensure!(event.run_id==run&&event.attempt_id==attempt,"foreign production adapter event");let matched=predicate(&event.kind);ensure!(!matches!(&event.kind,AgentLinkEventKind::Closed{..}),"production adapter closed before observation");ensure!(matched||!matches!(&event.kind,AgentLinkEventKind::CommandResult{..}),"unexpected production adapter result: {event:?}");ensure!(self.events.len()<256,"adapter event bound");self.events.push(event.clone());if matched{return Ok(event);}},Err(mpsc::RecvTimeoutError::Timeout)=>{},Err(e)=>return Err(e.into())}}}
    fn no_interrupt_confirmation(&mut self,events:&Receiver<AgentLinkEvent>,run:&str,attempt:&str)->Result<()> {let until=Instant::now()+Duration::from_millis(150);while Instant::now()<until{self.bounds()?;match events.recv_timeout(Duration::from_millis(10)){Ok(event)=>{ensure!(event.run_id==run&&event.attempt_id==attempt,"foreign hold event");ensure!(!matches!(&event.kind,AgentLinkEventKind::CommandResult{command_id,..} if command_id=="interrupt"),"receipt/ACK alone completed native interruption");self.events.push(event);},Err(mpsc::RecvTimeoutError::Timeout)=>{},Err(e)=>return Err(e.into())}}Ok(())}
    fn settle(&mut self)->Result<()> {let end=self.cleanup_end;let mut errors=Vec::new();self.stopping.store(true,Ordering::Release);if let Some(tap)=&self.tap{tap.stopped.store(true,Ordering::Release);}if let Some(actor)=&self.actor{actor.close();}
        if let Some(actor)=&mut self.actor{match remaining(end).and_then(|d|actor.await_closed(d)){Ok(joined)=>self.actor_join=Some(joined),Err(e)=>errors.push(e.to_string())}}
        if let Some(tap)=&mut self.tap{match tap.worker.as_ref().context("tap original worker absent").and_then(|w|w.await_joined(remaining(end)?)){Ok(joined)=>tap.joined=Some(joined),Err(e)=>errors.push(e.to_string())}}
        for child in [&mut self.bridge,&mut self.provider].into_iter().flatten(){let _original=send(&child.input,&json!({"op":"stop"}),end);}
        for child in [&mut self.bridge,&mut self.cli,&mut self.provider].into_iter().flatten(){if let Err(e)=child.request_stop(end).and_then(|_|child.settle(end)){errors.push(e.to_string());}}
        ensure!(errors.is_empty(),"actual child/pipe/thread custody remains retained: {}",errors.join("; "));Ok(())
    }
    fn receipt(&self)->Value {let relay=self.relay.lock().unwrap_or_else(|p|p.into_inner());let tap=self.tap.as_ref().map(|t|{let s=t.state.lock().unwrap_or_else(|p|p.into_inner());json!({"frames":s.frames,"real_interrupt_ack":s.ack,"original_terminal":s.held_value,"original_terminal_forwarded":s.forwarded,"error":s.error,"actual_join":t.joined.is_some()})});
        json!({"claude":self.cli.as_ref().map(RetainedChild::receipt),"bridge":self.bridge.as_ref().map(RetainedChild::receipt),"provider":self.provider.as_ref().map(RetainedChild::receipt),"actor_actually_joined":self.actor_join.is_some(),"tap":tap,"actual_cli_interrupt_receipts":relay.receipts,"forwarded_cli_receipt_ids":relay.forwarded_receipts,"actual_cli_original_terminal":relay.held,"actual_cli_terminal_released":relay.released,"actual_cli_peer_inputs":relay.peer_inputs,"adapter_events":self.events})}
}

fn execute(owner:&mut Owner,binary:&Path,node:&Path,provider_script:&Path,bridge_script:&Path,protocol:&Path,nonce:&str)->Result<()> {
    let home=owner.artifacts.join("home");let project=owner.artifacts.join("project");let tmp=owner.artifacts.join("tmp");for dir in [&home,&project,&tmp]{fs::create_dir(dir)?;fs::set_permissions(dir,fs::Permissions::from_mode(0o700))?;}
    let mut provider=Command::new(node);provider.args([provider_script.as_os_str(),std::ffi::OsStr::new(nonce),project.as_os_str()]).current_dir(&project).env_clear().env("HOME",&home).env("PATH","/usr/bin:/bin");
    RetainedChild::spawn_retaining(&mut owner.provider,&mut provider,node)?;owner.provider.as_mut().unwrap().readers(&owner.artifacts,"provider",true,None)?;
    let ready=owner.provider_until(|v|v["kind"]=="ready")?;let origin=ready["baseUrl"].as_str().context("actual provider origin absent")?.to_string();ensure!(origin.starts_with("http://127.0.0.1:")&&origin[17..].parse::<u16>().is_ok(),"provider is not exact loopback");
    let mut cli=Command::new(binary);cli.current_dir(&project).env_clear().env("HOME",&home).env("TMPDIR",&tmp).env("PATH","/usr/bin:/bin").env("LANG","C.UTF-8").env("TERM","dumb")
        .env("ANTHROPIC_API_KEY","sk-ant-api03-loopback-fixture-no-billing").env("ANTHROPIC_BASE_URL",&origin).env("DISABLE_AUTOUPDATER","1").env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC","1")
        .env("HTTP_PROXY","http://127.0.0.1:9").env("HTTPS_PROXY","http://127.0.0.1:9").env("ALL_PROXY","http://127.0.0.1:9").env("NO_PROXY","127.0.0.1,localhost")
        .args(["--print","--input-format","stream-json","--output-format","stream-json","--verbose","--replay-user-messages","--include-partial-messages","--permission-mode","bypassPermissions","--dangerously-skip-permissions","--setting-sources","","--strict-mcp-config","--mcp-config","{\"mcpServers\":{}}","--no-session-persistence","--model","claude-sonnet-4-6"]);
    RetainedChild::spawn_retaining(&mut owner.cli,&mut cli,binary)?;
    let real=owner.sockets.join("real.sock");let proxy=owner.sockets.join("cx.sock");let run=format!("claude-fixture-{nonce}");let attempt=uuid::Uuid::new_v4().to_string();
    let mut bridge=Command::new(node);bridge.args([bridge_script.as_os_str(),protocol.as_os_str(),real.as_os_str(),std::ffi::OsStr::new(&run),std::ffi::OsStr::new(&attempt),project.as_os_str()]).current_dir(&project).env_clear().env("HOME",&home).env("PATH","/usr/bin:/bin");
    RetainedChild::spawn_retaining(&mut owner.bridge,&mut bridge,node)?;
    let bridge_input=Arc::clone(&owner.bridge.as_ref().unwrap().input);let cli_input=Arc::clone(&owner.cli.as_ref().unwrap().input);let stopping=Arc::clone(&owner.stopping);let relay=Arc::clone(&owner.relay);let end=owner.run_end;
    let cli_forward:Forward=Arc::new(move|frame|{if stopping.load(Ordering::Acquire){return Ok(());}let mut state=relay.lock().unwrap();
        let receipt=if frame["type"]=="control_response"&&frame.pointer("/response/response/still_queued").is_some(){state.receipts.push(frame.clone());frame["response"]["request_id"].as_str().map(str::to_owned)}else{None};
        if state.hold_terminal&&frame["type"]=="result"&&matches!(frame["terminal_reason"].as_str(),Some("aborted_streaming"|"aborted_tools")){ensure!(state.held.is_none(),"multiple held original Claude terminals");state.held=Some(frame.clone());return Ok(());}drop(state);send(&bridge_input,&json!({"op":"cli_frame","frame":frame}),end)?;if let Some(id)=receipt{relay.lock().unwrap().forwarded_receipts.push(id);}Ok(())});
    owner.cli.as_mut().unwrap().readers(&owner.artifacts,"claude",true,Some(cli_forward))?;
    let stopping=Arc::clone(&owner.stopping);let relay=Arc::clone(&owner.relay);
    let bridge_forward:Forward=Arc::new(move|value|{if stopping.load(Ordering::Acquire){return Ok(());}if value["kind"]!="cli_input"{return Ok(());}let frame=&value["frame"];
        {let mut state=relay.lock().unwrap();if frame["type"]=="control_request"&&frame["request"]["subtype"]=="interrupt"{state.interrupts.push(frame["request_id"].as_str().context("actual interrupt ID absent")?.to_owned());}
            if frame["type"]=="user"&&frame["origin"]["kind"]=="peer"{state.peer_inputs.push(frame.clone());}}
        send(&cli_input,frame,end)});
    owner.bridge.as_mut().unwrap().readers(&owner.artifacts,"bridge",true,Some(bridge_forward))?;
    owner.bridge_until(|v|v["kind"]=="ready")?;owner.provider_until(|v|v["kind"]=="held"&&v["primary"]==2)?;
    let snapshot=owner.bridge_until(|v|v["kind"]=="snapshot"&&v["snapshot"]["qualified"]==true&&v["snapshot"]["activeTurnId"].is_string())?;
    let thread=snapshot["snapshot"]["sessionId"].as_str().context("actual CLI session absent")?.to_string();let first=snapshot["snapshot"]["activeTurnId"].as_str().context("actual CLI active turn absent")?.to_string();
    ensure!(fs::read(project.join("fixture.txt"))?==b"actual-claude-native-tool-edit\n","actual official CLI tool edit differs");
    Tap::start_into(&mut owner.tap,&proxy,real,thread.clone(),owner.cleanup_end)?;
    let (tx,rx)=mpsc::channel();owner.actor=Some(spawn_link(AgentLinkLaunch{format_version:1,run_id:run.clone(),attempt_id:attempt.clone(),backend:"claude-sdk".into(),socket_path:proxy,log_path:owner.artifacts.join("adapter.log")},&project,Some(thread.clone()),tx)?);
    owner.actor_until(&rx,|e|matches!(e,AgentLinkEventKind::Bound{thread_id,active_turn_id:Some(turn),steer_mode:AgentSteerMode::InterruptThenReplace,..} if thread_id==&thread&&turn==&first),&run,&attempt)?;
    owner.actor_until(&rx,|e|matches!(e,AgentLinkEventKind::ProfileObserved{qualified:true,uncertain:false}),&run,&attempt)?;
    let message=PeerMessage{sender_run_id:"authenticated-fixture-peer".into(),message_id:uuid::Uuid::new_v4().to_string(),text:format!("RUDDER_CLAUDE_NATIVE_PEER_{nonce}")};
    let make=|id:&str,operation:AgentLinkOperation,turn:Option<String>|AgentLinkCommand{command_id:id.into(),run_id:run.clone(),attempt_id:attempt.clone(),expected_thread_id:thread.clone(),expected_turn_id:turn,operation,timeout:Duration::from_secs(8)};
    owner.actor.as_ref().unwrap().submit(make("wrong-turn",AgentLinkOperation::Steer{message:message.clone()},Some("wrong-turn".into())))?;
    owner.actor_until(&rx,|e|matches!(e,AgentLinkEventKind::CommandResult{command_id,outcome:AgentLinkOutcome::Rejected{..}} if command_id=="wrong-turn"),&run,&attempt)?;
    owner.actor.as_ref().unwrap().submit(make("steer",AgentLinkOperation::Steer{message:message.clone()},Some(first.clone())))?;
    owner.actor_until(&rx,|e|matches!(e,AgentLinkEventKind::CommandResult{command_id,outcome:AgentLinkOutcome::Unsupported{..}} if command_id=="steer"),&run,&attempt)?;
    ensure!(owner.relay.lock().unwrap().peer_inputs.is_empty(),"unsupported replacement wrote peer input");
    let second=first.clone(); // Retain the actual original turn; no replacement exists.
    owner.relay.lock().unwrap().hold_terminal=true;owner.tap.as_ref().unwrap().state.lock().unwrap().hold_turn=Some(second.clone());
    owner.actor.as_ref().unwrap().submit(make("interrupt",AgentLinkOperation::Interrupt,Some(second.clone())))?;
    loop{owner.bounds()?;let state=owner.relay.lock().unwrap();if state.interrupts.len()==1&&state.held.is_some()&&state.forwarded_receipts.contains(&state.interrupts[0]){let id=&state.interrupts[0];ensure!(state.receipts.iter().any(|v|v["response"]["request_id"]==*id&&v["response"]["response"]["still_queued"]==json!([])),"actual matching CLI interrupt receipt absent");break;}drop(state);std::thread::sleep(Duration::from_millis(2));}
    // Forwarded actual CLI receipt alone must not finish the original command.
    owner.no_interrupt_confirmation(&rx,&run,&attempt)?;
    let original=owner.relay.lock().unwrap().held.clone().context("original CLI terminal absent")?;
    send(&owner.bridge.as_ref().unwrap().input,&json!({"op":"cli_frame","frame":original}),owner.run_end)?;owner.relay.lock().unwrap().released=true;
    loop{owner.bounds()?;let tap=owner.tap.as_ref().unwrap().state.lock().unwrap();ensure!(tap.error.is_none(),"actual tap failed");if tap.ack&&tap.held_value.is_some(){break;}drop(tap);std::thread::sleep(Duration::from_millis(2));}
    // Now actual bridge {} ACK reached Rust, but only the original lifecycle
    // notification remains held. No invented replacement/stop notification.
    owner.no_interrupt_confirmation(&rx,&run,&attempt)?;owner.tap.as_ref().unwrap().state.lock().unwrap().release=true;
    owner.actor_until(&rx,|e|matches!(e,AgentLinkEventKind::CommandResult{command_id,outcome:AgentLinkOutcome::InterruptConfirmed{turn_id}} if command_id=="interrupt"&&turn_id==&second),&run,&attempt)?;
    owner.actor.as_ref().unwrap().submit(make("stale-steer",AgentLinkOperation::Steer{message:message.clone()},Some(second.clone())))?;
    owner.actor_until(&rx,|e|matches!(e,AgentLinkEventKind::CommandResult{command_id,outcome:AgentLinkOutcome::Rejected{..}} if command_id=="stale-steer"),&run,&attempt)?;
    owner.actor.as_ref().unwrap().submit(make("resume",AgentLinkOperation::Resume{message},Some(second)))?;
    owner.actor_until(&rx,|e|matches!(e,AgentLinkEventKind::CommandResult{command_id,outcome:AgentLinkOutcome::Unsupported{..}} if command_id=="resume"),&run,&attempt)?;
    let tap=owner.tap.as_ref().unwrap().state.lock().unwrap();ensure!(tap.frames.iter().filter(|v|v["direction"]=="adapter_to_claude"&&v["frame"]["method"]=="turn/steer").count()==0&&tap.frames.iter().filter(|v|v["direction"]=="adapter_to_claude"&&v["frame"]["method"]=="turn/interrupt").count()==1,"wrong/stale native controls escaped");ensure!(!tap.frames.iter().any(|v|v["direction"]=="adapter_to_claude"&&matches!(v["frame"]["method"].as_str(),Some("thread/start"|"thread/fork"|"turn/start"))),"native adapter created substitute conversation/turn");Ok(())
}

#[test]
#[ignore="actual retained signed Claude2.1.316 + compiled production protocol + Node; synthetic loopback only; Root runs"]
fn actual_claude_loopback_adapter_controls() {
    let result=(||->Result<()>{
        let binary=input("RUDDER_REAL_CLAUDE_BIN",false)?;let node=input("RUDDER_CLAUDE_PROTOCOL_NODE",false)?;let provider=input("RUDDER_CLAUDE_PROTOCOL_PROVIDER",false)?;
        let bridge=input("RUDDER_CLAUDE_PROTOCOL_BRIDGE",false)?;let protocol=input("RUDDER_CLAUDE_PROTOCOL_MODULE",false)?;let provenance=input("RUDDER_CLAUDE_PROTOCOL_PROVENANCE",false)?;let root=input("RUDDER_CLAUDE_PROTOCOL_ARTIFACT_ROOT",true)?;
        let expected=std::env::var("RUDDER_CLAUDE_PROTOCOL_MODULE_SHA256").context("actual compiled protocol SHA required")?;ensure!(expected.len()==64&&expected.bytes().all(|b|b.is_ascii_hexdigit()&&!b.is_ascii_uppercase())&&expected.bytes().any(|b|b!=b'0'),"compiled protocol SHA unconfigured");
        let m=fs::symlink_metadata(&root)?;ensure!(m.uid()==unsafe{libc::geteuid()}&&m.mode()&0o777==0o700,"artifact root is not private");
        ensure!(hash_file(&binary)?==(ELF_SHA.to_owned(),ELF_BYTES),"official retained Claude ELF differs");ensure!(hash_file(&provider)?.0==PROVIDER_SHA&&hash_file(&bridge)?.0==BRIDGE_SHA,"fixture source differs");ensure!(hash_file(&protocol)?.0==expected,"actual production protocol module differs");
        ensure!(fs::metadata(&provenance)?.len()<=65536,"acquisition receipt bound");let receipt:Value=serde_json::from_slice(&fs::read(&provenance)?)?;
        ensure!(receipt["signatureVerified"]==true&&receipt["binaryMatchesSignedManifest"]==true&&receipt["binarySha256"]==ELF_SHA&&receipt["binarySize"]==ELF_BYTES,"acquisition DATA differs; it cannot replace actual ELF hash");
        ensure!(free_bytes(&root)?>FREE_FLOOR+16*1024*1024,"fixture free-space floor");let started=Instant::now();let nonce=uuid::Uuid::new_v4().to_string();let artifacts=root.join(format!("native-claude-{nonce}"));let sockets=PathBuf::from("/tmp").join(format!("rudder-cl-{}",uuid::Uuid::new_v4().simple()));
        for dir in [&artifacts,&sockets]{fs::create_dir(dir)?;fs::set_permissions(dir,fs::Permissions::from_mode(0o700))?;}
        let mut owner=Box::new(Owner {artifacts,sockets,run_end:started+Duration::from_secs(70),cleanup_end:started+Duration::from_secs(95),cli:None,bridge:None,provider:None,stopping:Arc::new(AtomicBool::new(false)),relay:Arc::new(Mutex::new(CliRelay::default())),tap:None,actor:None,actor_join:None,events:Vec::new()});
        let outcome=std::panic::catch_unwind(std::panic::AssertUnwindSafe(||execute(&mut owner,&binary,&node,&provider,&bridge,&protocol,&nonce)));
        let cleanup=std::panic::catch_unwind(std::panic::AssertUnwindSafe(||owner.settle())).unwrap_or_else(|_|Err(anyhow::anyhow!("cleanup panicked; actual carrier retained")));
        let final_inputs=(||->Result<()>{ensure!(hash_file(&binary)?==(ELF_SHA.to_owned(),ELF_BYTES)&&hash_file(&provider)?.0==PROVIDER_SHA&&hash_file(&bridge)?.0==BRIDGE_SHA&&hash_file(&protocol)?.0==expected,"fixture inputs changed");Ok(())})();
        let passed=matches!(&outcome,Ok(Ok(())))&&cleanup.is_ok()&&final_inputs.is_ok();let record=json!({"format_version":1,"scope":"actual-rust-claude-adapter-synthetic-loopback-only","qualified":passed,"claude_supported":false,
            "paid_model_calls":0,"production_startup":false,"production_ts_renderer":false,"managed_guest":false,"spec3":false,"managed_backend_witness":false,"model_qualification":false,"network_isolation":false,
            "elf_sha256":ELF_SHA,"provider_sha256":PROVIDER_SHA,"bridge_sha256":BRIDGE_SHA,"production_protocol_sha256":expected,"fixture_rpc_relay":true,
            "run_ok":matches!(&outcome,Ok(Ok(()))),"run_error":match &outcome{Ok(Err(e))=>Some(format!("{e:#}")),Err(_)=>Some("fixture panicked".to_owned()),_=>None},"cleanup_ok":cleanup.is_ok(),"cleanup_error":cleanup.as_ref().err().map(|e|format!("{e:#}")),"final_inputs_ok":final_inputs.is_ok(),"actors_and_processes":owner.receipt()});
        let written=(||->Result<()>{let bytes=serde_json::to_vec_pretty(&record)?;ensure!(disk_bytes(&owner.artifacts)?.checked_add(bytes.len() as u64).is_some_and(|n|n<=ARTIFACT_CAP),"final artifact bound");ensure!(free_bytes(&owner.artifacts)?>FREE_FLOOR+bytes.len() as u64,"final free-space floor");let mut f=private_file(&owner.artifacts.join("report.json"))?;f.write_all(&bytes)?;f.sync_data()?;Ok(())})();
        if cleanup.is_err(){let path=owner.artifacts.clone();let _actual=Box::leak(owner);bail!("actual fixture carrier retained until native process teardown at {}",path.display());}
        written?;match outcome{Ok(result)=>result?,Err(_)=>bail!("fixture panicked; original resources settled")};final_inputs?;
        println!("{}",json!({"scope":"actual-rust-claude-adapter-synthetic-loopback-only","qualified":true,"report":owner.artifacts.join("report.json"),"claude_supported":false,"paid_model_calls":0}));Ok(())})();
    if let Err(e)=result{panic!("{e:#}");}
}
