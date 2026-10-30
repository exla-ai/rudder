use super::*;
use std::{fs, sync::Mutex, os::unix::net::UnixListener};

const KEY: &str = "rudder-synthetic-fixture-key-no-real-provider";
fn key() -> ManagedCodexApiKey {
    crate::managed_launch::SelectedCredentials::api_key("fixture".into(), crate::Backend::Codex, KEY.into()).unwrap()
        .take_host_codex_key().unwrap().unwrap()
}
struct Process { expected:u32, pidfd:OwnedFd, checks:std::sync::atomic::AtomicUsize }
impl Process {
    fn own() -> Self { Self::for_pid(std::process::id()) }
    fn for_pid(pid:u32) -> Self { let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        assert!(fd >= 0); Self { expected:pid, pidfd:unsafe { OwnedFd::from_raw_fd(fd as i32) }, checks:std::sync::atomic::AtomicUsize::new(0) } }
}
impl OwnedPeerProcess for Process {
    fn check_current(&self) -> anyhow::Result<()> { let mut event = libc::pollfd { fd:self.pidfd.as_raw_fd(), events:libc::POLLIN, revents:0 };
        anyhow::ensure!(unsafe { libc::poll(&mut event, 1, 0) } == 0, "fixture process exited"); Ok(()) }
    fn verify_peer(&self, stream: &UnixStream) -> anyhow::Result<()> { self.check_current()?;
        let mut peer:libc::ucred = unsafe { mem::zeroed() }; let mut size = mem::size_of_val(&peer) as libc::socklen_t;
        anyhow::ensure!(unsafe { libc::getsockopt(stream.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED, (&mut peer as *mut libc::ucred).cast(), &mut size) } == 0
            && peer.pid == self.expected as i32 && peer.uid == unsafe { libc::geteuid() }, "fixture peer differs from held process");
        self.checks.fetch_add(1, Ordering::AcqRel); Ok(()) }
}
#[derive(Clone, Copy)]
enum Behavior { Good, FileStorage, NullAccount, OtherAccount, Reject, HoldLogin, SlowHandshake }
struct Server { root:std::path::PathBuf, path:std::path::PathBuf, methods:Arc<Mutex<Vec<String>>>, login:Arc<AtomicBool>, stop:Arc<AtomicBool>, worker:Option<std::thread::JoinHandle<()>> }
impl Server {
    fn start(behavior:Behavior, delay:Duration) -> Self {
        let root = std::env::temp_dir().join(format!("rudder-auth-{}", uuid::Uuid::new_v4())); fs::create_dir(&root).unwrap();
        let path = root.join("cx.sock"); let socket_path = path.clone();
        let methods = Arc::new(Mutex::new(Vec::new())); let observed = methods.clone();
        let login = Arc::new(AtomicBool::new(false)); let saw_login = login.clone();
        let stop = Arc::new(AtomicBool::new(false)); let stopped = stop.clone();
        let worker = std::thread::spawn(move || {
            std::thread::sleep(delay); if stopped.load(Ordering::Acquire) { return; }
            let listener = UnixListener::bind(socket_path).unwrap(); listener.set_nonblocking(true).unwrap();
            let deadline = Instant::now()+Duration::from_secs(3);
            let stream = loop { if stopped.load(Ordering::Acquire) || Instant::now() >= deadline { return; }
                match listener.accept() { Ok((stream,_)) => break stream, Err(error) if error.kind() == io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(5)),
                    Err(error) => panic!("fixture accept failed: {error}") } };
            stream.set_nonblocking(false).unwrap(); stream.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
            stream.set_write_timeout(Some(Duration::from_millis(50))).unwrap();
            if matches!(behavior, Behavior::SlowHandshake) { while !stopped.load(Ordering::Acquire) && Instant::now() < deadline { std::thread::sleep(Duration::from_millis(5)); } return; }
            let Ok(mut socket) = tungstenite::accept(stream) else { return; };
            while !stopped.load(Ordering::Acquire) && Instant::now() < deadline {
                let message = match socket.read() { Ok(Message::Text(bytes)) => serde_json::from_str::<Value>(&bytes).unwrap(),
                    Err(tungstenite::Error::Io(error)) if matches!(error.kind(), io::ErrorKind::WouldBlock|io::ErrorKind::TimedOut) => continue,
                    _ => break };
                let method = message["method"].as_str().unwrap(); observed.lock().unwrap().push(method.into());
                let Some(id) = message.get("id") else { continue; };
                let result = match method {
                    "initialize" => json!({"userAgent":"rudder_agent_link/0.166.0"}),
                    "config/read" => json!({"config":{"cli_auth_credentials_store":if matches!(behavior,Behavior::FileStorage) { "file" } else { "ephemeral" }}}),
                    "account/login/start" => {
                        assert_eq!(message["params"]["type"], "apiKey"); assert_eq!(message["params"]["apiKey"], KEY);
                        saw_login.store(true, Ordering::Release);
                        if matches!(behavior,Behavior::HoldLogin) { continue; }
                        if matches!(behavior,Behavior::Reject) { let _ = socket.send(Message::Text(json!({"id":id,"error":{"message":KEY}}).to_string())); continue; }
                        let _ = socket.send(Message::Text(json!({"method":"account/login/completed","params":{"success":true}}).to_string()));
                        json!({"type":"apiKey"})
                    }
                    "account/read" => json!({"requiresOpenaiAuth":true,"account":match behavior { Behavior::NullAccount => Value::Null,
                        Behavior::OtherAccount => json!({"type":"chatgpt"}), _ => json!({"type":"apiKey"}) }}),
                    _ => panic!("bootstrap attempted a non-auth RPC"),
                };
                if socket.send(Message::Text(json!({"id":id,"result":result}).to_string())).is_err() { break; }
            }
        });
        Self { root,path,methods,login,stop,worker:Some(worker) }
    }
    fn run(&self, process:&Process, cancelled:&AtomicBool, duration:Duration) -> Result<()> { bootstrap(&self.path,process,&key(),cancelled,Instant::now()+duration) }
    fn logins(&self) -> usize { self.methods.lock().unwrap().iter().filter(|method|method.as_str()=="account/login/start").count() }
}
impl Drop for Server { fn drop(&mut self) { self.stop.store(true, Ordering::Release); if let Some(worker)=self.worker.take() { worker.join().unwrap(); } let _=fs::remove_dir_all(&self.root); } }

#[test]
fn selected_key_authenticates_once_only_after_ephemeral_config_and_exact_peer() {
    let server=Server::start(Behavior::Good,Duration::ZERO); let process=Process::own();
    server.run(&process,&AtomicBool::new(false),Duration::from_secs(2)).unwrap();
    assert_eq!(*server.methods.lock().unwrap(), ["initialize","initialized","config/read","account/login/start","account/read"]);
    assert_eq!(server.logins(),1); assert_eq!(process.checks.load(Ordering::Acquire),2);
}
#[test]
fn readiness_may_precede_listener_but_connection_retries_never_replay_login() {
    let server=Server::start(Behavior::Good,Duration::from_millis(80)); let process=Process::own();
    server.run(&process,&AtomicBool::new(false),Duration::from_secs(2)).unwrap();
    assert_eq!(server.logins(),1); assert_eq!(process.checks.load(Ordering::Acquire),2);
}
#[test]
fn selected_key_never_leaves_host_for_wrong_storage_or_foreign_peer() {
    let server=Server::start(Behavior::FileStorage,Duration::ZERO);
    assert_eq!(server.run(&Process::own(),&AtomicBool::new(false),Duration::from_secs(2)),Err(ManagedCodexAuthError::Storage)); assert_eq!(server.logins(),0);
    let server=Server::start(Behavior::Good,Duration::ZERO); let mut process=Process::own(); process.expected+=1;
    assert_eq!(server.run(&process,&AtomicBool::new(false),Duration::from_secs(2)),Err(ManagedCodexAuthError::Binding));
    assert!(server.methods.lock().unwrap().is_empty()); assert_eq!(server.logins(),0);
}
#[test]
fn login_reply_or_notification_cannot_replace_observed_api_key_account() {
    for behavior in [Behavior::NullAccount,Behavior::OtherAccount] { let server=Server::start(behavior,Duration::ZERO);
        assert_eq!(server.run(&Process::own(),&AtomicBool::new(false),Duration::from_secs(2)),Err(ManagedCodexAuthError::Account)); assert_eq!(server.logins(),1); }
}
#[test]
fn rejection_and_uncertain_timeout_are_sanitized_and_never_retry_login() {
    let server=Server::start(Behavior::Reject,Duration::ZERO);
    let failure=server.run(&Process::own(),&AtomicBool::new(false),Duration::from_secs(2)).unwrap_err();
    assert_eq!(failure,ManagedCodexAuthError::Rejected); assert!(!format!("{failure:?} {failure}").contains(KEY)); assert_eq!(server.logins(),1);
    let server=Server::start(Behavior::HoldLogin,Duration::ZERO);
    assert_eq!(server.run(&Process::own(),&AtomicBool::new(false),Duration::from_millis(150)),Err(ManagedCodexAuthError::Deadline)); assert_eq!(server.logins(),1);
}
#[test]
fn cancellation_and_uncooperative_handshake_keep_total_deadline() {
    let server=Server::start(Behavior::Good,Duration::ZERO);
    assert_eq!(server.run(&Process::own(),&AtomicBool::new(true),Duration::from_secs(1)),Err(ManagedCodexAuthError::Cancelled)); assert_eq!(server.logins(),0);
    let server=Server::start(Behavior::SlowHandshake,Duration::ZERO); let began=Instant::now();
    assert_eq!(server.run(&Process::own(),&AtomicBool::new(false),Duration::from_millis(100)),Err(ManagedCodexAuthError::Deadline));
    assert!(began.elapsed()<Duration::from_secs(1)); assert_eq!(server.logins(),0);
    let server=Server::start(Behavior::HoldLogin,Duration::ZERO); let cancelled=Arc::new(AtomicBool::new(false)); let cancel=cancelled.clone(); let login=server.login.clone();
    let waiter=std::thread::spawn(move || { let deadline=Instant::now()+Duration::from_secs(1);
        while !login.load(Ordering::Acquire) && Instant::now()<deadline { std::thread::sleep(Duration::from_millis(1)); } cancel.store(true,Ordering::Release); });
    assert_eq!(server.run(&Process::own(),&cancelled,Duration::from_secs(2)),Err(ManagedCodexAuthError::Cancelled)); waiter.join().unwrap(); assert_eq!(server.logins(),1);
}

#[test]
#[ignore = "explicit pinned public binary path; synthetic local auth only, no inference"]
fn pinned_codex_0166_synthetic_ephemeral_auth_contract() {
    use std::{io::Read, os::unix::{fs::PermissionsExt, process::ExitStatusExt}, process::{Command, Stdio}};
    use sha2::{Digest,Sha256};
    fn hash_file(path:impl AsRef<Path>) -> String { let mut file=fs::File::open(path).unwrap(); let mut hash=Sha256::new(); let mut buffer=[0;64*1024];
        loop { let count=file.read(&mut buffer).unwrap(); if count==0 { break; } hash.update(&buffer[..count]); } format!("{:x}",hash.finalize()) }
    let binary=std::env::var_os("RUDDER_MANAGED_CODEX_TEST_BINARY").expect("explicit pinned binary path required");
    let root=std::path::PathBuf::from("/workspace").join(format!("rudder-codex-auth-{}",uuid::Uuid::new_v4()));
    fs::create_dir(&root).unwrap(); fs::set_permissions(&root,fs::Permissions::from_mode(0o700)).unwrap();
    let home=root.join("home"); let codex_home=root.join("codex"); fs::create_dir(&home).unwrap(); fs::create_dir(&codex_home).unwrap();
    let version=Command::new(&binary).arg("--version").env_clear().env("HOME",&home).env("CODEX_HOME",&codex_home).output().unwrap();
    assert!(version.status.success()); let version=String::from_utf8(version.stdout).unwrap(); assert_eq!(version.trim(),"codex-cli 0.166.0");
    let executable_sha256=hash_file(&binary);
    let log_path=root.join("server.log"); let log=fs::File::create(&log_path).unwrap();
    let socket=root.join("cx.sock");
    let child=Command::new(binary).args(["-c","cli_auth_credentials_store=\"ephemeral\"","-c","analytics.enabled=false","app-server","--listen"])
        .arg(format!("unix://{}",socket.display())).env_clear().env("HOME",&home).env("CODEX_HOME",&codex_home).env("RUST_LOG","error")
        .current_dir(&home).stdin(Stdio::null()).stdout(Stdio::from(log.try_clone().unwrap())).stderr(Stdio::from(log)).spawn().unwrap();
    struct ChildGuard(Option<std::process::Child>); impl Drop for ChildGuard { fn drop(&mut self) { if let Some(child)=&mut self.0 { let _=child.kill(); let _=child.wait(); } } }
    let mut child=ChildGuard(Some(child)); let child_pid=child.0.as_ref().unwrap().id(); let process=Process::for_pid(child_pid);
    assert_eq!(hash_file(format!("/proc/{child_pid}/exe")),executable_sha256,"evidence must identify the actual running executable");
    let result=bootstrap(&socket,&process,&key(),&AtomicBool::new(false),Instant::now()+Duration::from_secs(15));
    let kill_requested=child.0.as_mut().unwrap().kill().is_ok();
    let exit=child.0.as_mut().unwrap().wait().unwrap(); child.0=None;
    assert!(kill_requested); assert_eq!(exit.signal(),Some(libc::SIGKILL));
    assert!(result.is_ok(), "bounded synthetic bootstrap failed: {result:?}; inspect only owned redacted fixture logs at {}",root.display());
    assert!(!codex_home.join("auth.json").exists(), "ephemeral login must not write auth.json");
    let scan_deadline=Instant::now()+Duration::from_secs(15);
    let mut pending=vec![root.clone()]; let mut files=0; let mut entries=0; let mut total=0u64;
    while let Some(directory)=pending.pop() { for entry in fs::read_dir(directory).unwrap() { let entry=entry.unwrap(); entries+=1; assert!(entries<=16_384 && Instant::now()<scan_deadline);
        if entry.file_type().unwrap().is_symlink() { continue; }
        let metadata=entry.metadata().unwrap();
        if metadata.is_dir() { pending.push(entry.path()); } else if metadata.is_file() { files+=1; assert!(files<=8_192 && metadata.len()<=64*1024*1024);
            let mut file=fs::File::open(entry.path()).unwrap(); let mut bytes=[0u8;64*1024]; let mut retained=0; let mut file_bytes=0u64;
            loop { assert!(Instant::now()<scan_deadline); let count=file.read(&mut bytes[retained..]).unwrap(); if count==0 { break; }
                file_bytes+=count as u64; total+=count as u64; assert!(file_bytes<=64*1024*1024 && total<=256*1024*1024);
                let length=retained+count;
                assert!(!bytes[..length].windows(KEY.len()).any(|bytes|bytes==KEY.as_bytes()),"synthetic credential reached persisted fixture data");
                retained=(KEY.len()-1).min(length); bytes.copy_within(length-retained..length,0); } } } }
    let seconds=std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as libc::time_t;
    let mut utc:libc::tm=unsafe { mem::zeroed() }; assert!(!unsafe { libc::gmtime_r(&seconds,&mut utc) }.is_null());
    let utc=format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",utc.tm_year+1900,utc.tm_mon+1,utc.tm_mday,utc.tm_hour,utc.tm_min,utc.tm_sec);
    fs::write(root.join("result.json"),serde_json::to_vec_pretty(&json!({"format_version":1,"utc":utc,"binary_version":version.trim(),"executable_sha256":executable_sha256,
        "storage":"ephemeral","account_type":"apiKey","kernel_peer_pid_matches_owned_child":true,
        "child_cleanup":{"kill_requested":kill_requested,"wait_completed":true,"exit_signal":exit.signal()},"auth_json_created":false,"synthetic_key_persisted":false,
        "persisted_fixture_scan":{"files":files,"bytes":total,"all_regular_files":true,"symlinks_followed":false,"buffer_bytes":65536,
            "max_files":8192,"max_entries":16384,"max_file_bytes":67108864,"max_total_bytes":268435456,"deadline_seconds":15},
        "rpc_methods":["initialize","config/read","account/login/start","account/read"],"model_calls":0,"thread_calls":0,"real_credentials":false,
        "network_isolation_tested":false,"managed_helper_tested":false,"provider_inference_tested":false,
        "qualification":"standalone pinned-binary local auth contract; not managed-helper or provider inference qualification"})).unwrap()).unwrap();
    println!("synthetic Codex auth evidence: {}",root.display());
}
