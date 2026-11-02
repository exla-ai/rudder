//! Actual pipe transport tests only. No SDK owner/result/Root ACK is issued.
//! Strict Root transition remains ignored until the separately reviewed gate.
use super::*;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt,PermissionsExt};
const ROLE:&str="RUDDER_SDK_HELPER_PIPE_TEST_ROLE";
const CHILD_DONE:&str="RUDDER_SDK_HELPER_ROOT_PIPE_CHILD_V1=passed";
const ROOT_DONE:&str="RUDDER_SDK_HELPER_ROOT_PIPE_TRANSPORT_V1=passed";
fn failure()->anyhow::Error{
    anyhow::anyhow!("private synthetic cause must never appear in diagnostic DATA")
        .context(BootstrapCheck::SdkHelperParentImage)
}
fn expected_frame()->Vec<u8>{
    let error=failure();
    let mut raw=serde_json::to_vec(&storage::retained_failure_data("compiler_sdk_helper","handshake",Some(&error))).unwrap();raw.push(b'\n');raw
}
fn flags(fd:i32)->i32{let result=unsafe{libc::fcntl(fd,libc::F_GETFL)};assert!(result>=0);result}
fn require_ignored_sigpipe(){
    let mut action:libc::sigaction=unsafe{std::mem::zeroed()};
    assert_eq!(unsafe{libc::sigaction(libc::SIGPIPE,std::ptr::null(),&mut action)},0);
    assert_eq!(action.sa_sigaction,libc::SIG_IGN,"Rust test runtime must already ignore SIGPIPE; test never changes it");
}
fn pipe(nonblocking:bool)->(OwnedFd,OwnedFd){
    let mut fds=[-1;2];assert_eq!(unsafe{libc::pipe2(fds.as_mut_ptr(),libc::O_CLOEXEC|if nonblocking{libc::O_NONBLOCK}else{0})},0);
    unsafe{(OwnedFd::from_raw_fd(fds[0]),OwnedFd::from_raw_fd(fds[1]))}
}
#[test]
fn helper_pipe_full_refuses_once_without_changing_shared_flags_or_bytes(){
    require_ignored_sigpipe();let(read,write)=pipe(true);let before=flags(write.as_raw_fd());
    let chunk=[b'x';4096];let mut total=0usize;
    loop{let n=unsafe{libc::write(write.as_raw_fd(),chunk.as_ptr().cast(),chunk.len())};
        if n<0{assert_eq!(io::Error::last_os_error().kind(),io::ErrorKind::WouldBlock);break}
        assert_eq!(n,4096);total+=n as usize;assert!(total<=16*1024*1024,"fixture pipe capacity exceeded finite bound");
    }
    assert!(!super::super::helper::try_helper_diagnostic_write(write.as_raw_fd(),b"fixed\n"));
    assert_eq!(flags(write.as_raw_fd()),before);drop(write);
    let mut seen=0;let mut bytes=[0u8;4096];loop{
        let n=unsafe{libc::read(read.as_raw_fd(),bytes.as_mut_ptr().cast(),bytes.len())};assert!(n>=0);
        if n==0{break}assert!(bytes[..n as usize].iter().all(|byte|*byte==b'x'));seen+=n as usize;
    }assert_eq!(seen,total);
}
#[test]
fn helper_pipe_closed_refuses_without_flag_changes_or_sigpipe_mutation(){
    require_ignored_sigpipe();let(read,write)=pipe(true);let before=flags(write.as_raw_fd());drop(read);
    assert!(!super::super::helper::try_helper_diagnostic_write(write.as_raw_fd(),b"fixed\n"));
    assert_eq!(flags(write.as_raw_fd()),before);require_ignored_sigpipe();
}
#[test]
fn helper_pipe_requires_existing_nonblocking_writable_fifo_and_bounded_input(){
    require_ignored_sigpipe();let(read,write)=pipe(false);let before=flags(write.as_raw_fd());
    assert!(!super::super::helper::try_helper_diagnostic_write(write.as_raw_fd(),b"fixed\n"));
    assert_eq!(flags(write.as_raw_fd()),before);drop(read);drop(write);
    let(read,write)=pipe(true);let before=flags(write.as_raw_fd());let read_before=flags(read.as_raw_fd());
    assert!(!super::super::helper::try_helper_diagnostic_write(read.as_raw_fd(),b"fixed\n"));
    assert!(!super::super::helper::try_helper_diagnostic_write(write.as_raw_fd(),&[]));
    assert!(!super::super::helper::try_helper_diagnostic_write(write.as_raw_fd(),&[0u8;4097]));
    assert_eq!(flags(write.as_raw_fd()),before);assert_eq!(flags(read.as_raw_fd()),read_before);
    let bytes=[b'z';4096];assert!(super::super::helper::try_helper_diagnostic_write(write.as_raw_fd(),&bytes));
    assert_eq!(flags(write.as_raw_fd()),before);drop(write);let mut actual=[0u8;4096];
    assert_eq!(unsafe{libc::read(read.as_raw_fd(),actual.as_mut_ptr().cast(),actual.len())},4096);assert_eq!(actual,bytes);
    assert_eq!(unsafe{libc::read(read.as_raw_fd(),actual.as_mut_ptr().cast(),actual.len())},0);
}
fn root_pipe(fd:i32)->Result<()>{
    let mut metadata:libc::stat=unsafe{std::mem::zeroed()};
    ensure!(unsafe{libc::fstat(fd,&mut metadata)}==0,"actual stderr metadata unavailable");
    ensure!(metadata.st_mode&libc::S_IFMT==libc::S_IFIFO&&metadata.st_uid==0&&metadata.st_gid==0&&metadata.st_mode&0o7777==0o600,"actual stderr is not the original Root0600 FIFO");Ok(())
}
fn strict_root()->Result<()>{
    storage::floor(std::process::id(),0)?;wire::require_initial_id_maps(std::process::id())?;
    let status=String::from_utf8(storage::proc_bytes("/proc/thread-self/status",16384)?)?;
    let cap=|key:&str|->Result<u64>{Ok(u64::from_str_radix(status.lines().find_map(|row|row.strip_prefix(key)).context("strict Root capability field missing")?.trim(),16)?)};
    let required=(1u64<<6)|(1u64<<7)|(1u64<<8);
    ensure!(cap("CapEff:")?&required==required&&cap("CapBnd:")?!=0,"strict Root requires existing SETGID/SETUID/SETPCAP and nonempty bounding set");
    ensure!(status.lines().any(|row|row.strip_prefix("NoNewPrivs:").is_some_and(|value|value.trim()=="0")),"strict Root must start NNP0");Ok(())
}
fn probe()->Result<()>{
    storage::floor(std::process::id(),1500)?;wire::require_initial_id_maps(std::process::id())?;
    ensure!(unsafe{libc::prctl(libc::PR_SET_DUMPABLE,0,0,0,0)}==0,"actual helper ND0 failed");
    require_protected_descriptor_directory(std::process::id())?;root_pipe(libc::STDERR_FILENO)?;require_ignored_sigpipe();
    let before=flags(libc::STDERR_FILENO);ensure!(before&libc::O_NONBLOCK!=0,"production pre_exec did not establish nonblocking stderr");
    // Wait for ORIGINAL Root parent to capture actual Child/pidfd/start/pipes.
    let mut released=[0u8;1];std::io::stdin().read_exact(&mut released)?;ensure!(released==[1],"original parent release differs");
    let reopened=unsafe{libc::open(c"/proc/self/fd/2".as_ptr(),libc::O_WRONLY|libc::O_NONBLOCK|libc::O_CLOEXEC|libc::O_NOCTTY)};
    if reopened>=0{unsafe{libc::close(reopened)};anyhow::bail!("zero-cap helper unexpectedly reopened Root0600 stderr")}
    let denied=io::Error::last_os_error();ensure!(denied.raw_os_error()==Some(libc::EACCES),"actual procfd reopen did not fail at DAC EACCES");
    let error=failure();
    // Exercise the UNCHANGED production generic emitter first. Root validates
    // exact stderr equality, so any old-path bytes would fail this regression.
    storage::emit_retained_failure("compiler_sdk_helper","handshake",Some(&error));
    ensure!(flags(libc::STDERR_FILENO)==before,"generic emitter changed inherited stderr flags");
    ensure!(super::super::helper::emit_helper_failure(&error),"actual fixed helper emitter failed inherited Root pipe write");
    ensure!(flags(libc::STDERR_FILENO)==before,"fixed emitter changed inherited stderr flags");
    println!("\n{CHILD_DONE}");std::io::stdout().flush()?;Ok(())
}
struct OriginalChild{
    child:Child,stdout:Option<std::process::ChildStdout>,stderr:Option<std::process::ChildStderr>,
    pidfd:Option<OwnedFd>,process:Option<Process>,start:Option<String>,out:Vec<u8>,err:Vec<u8>,
    out_eof:bool,err_eof:bool,exit:Option<ExitStatus>,failure:Option<anyhow::Error>,kill_sent:bool,
}
impl OriginalChild{
    fn fail(&mut self,error:anyhow::Error){if self.failure.is_none(){self.failure=Some(error)}}
    fn drain(fd:i32,raw:&mut Vec<u8>,eof:&mut bool)->Result<()>{
        if *eof{return Ok(())}let mut poll=libc::pollfd{fd,events:libc::POLLIN,revents:0};
        let ready=unsafe{libc::poll(&mut poll,1,0)};
        if ready<0{let error=io::Error::last_os_error();if error.kind()==io::ErrorKind::Interrupted{return Ok(())}return Err(error.into())}
        ensure!(poll.revents&(libc::POLLERR|libc::POLLNVAL)==0,"original pipe poll failed");if ready==0{return Ok(())}
        let mut bytes=[0u8;4096];let n=unsafe{libc::read(fd,bytes.as_mut_ptr().cast(),bytes.len())};
        if n<0{let error=io::Error::last_os_error();if matches!(error.kind(),io::ErrorKind::Interrupted|io::ErrorKind::WouldBlock){return Ok(())}return Err(error.into())}
        if n==0{*eof=true;return Ok(())}let room=16384usize.saturating_sub(raw.len());raw.extend_from_slice(&bytes[..(n as usize).min(room)]);
        ensure!(n as usize<=room,"original pipe finite capture bound exceeded");Ok(())
    }
    fn run(mut command:Command)->Result<Self>{
        let started=Instant::now();let child=command.spawn()?;
        // Store SAME Child before fallible custody work. No assertion/unwrap
        // can unwind its owner before actual wait AND both original EOFs.
        let mut owner=Self{child,stdout:None,stderr:None,pidfd:None,process:None,start:None,out:Vec::new(),err:Vec::new(),out_eof:false,err_eof:false,exit:None,failure:None,kill_sent:false};
        owner.stdout=owner.child.stdout.take();owner.stderr=owner.child.stderr.take();
        let capture=(||->Result<()>{
            ensure!(owner.stdout.is_some()&&owner.stderr.is_some(),"original pipe handles absent");
            root_pipe(owner.stderr.as_ref().unwrap().as_raw_fd())?;
            let raw=unsafe{libc::syscall(libc::SYS_pidfd_open,owner.child.id(),0)}as i32;
            if raw<0{return Err(io::Error::last_os_error().into())}owner.pidfd=Some(unsafe{OwnedFd::from_raw_fd(raw)});
            owner.process=Some(Process::capture(owner.pidfd.as_ref().unwrap().try_clone()?)?);owner.start=Some(original_stat(owner.child.id())?.1);
            owner.child.stdin.as_mut().context("original release pipe absent")?.write_all(&[1])?;owner.child.stdin.take();Ok(())
        })();if let Err(error)=capture{owner.fail(error)}
        loop{
            if started.elapsed()>=Duration::from_secs(20){owner.fail(anyhow::anyhow!("same original test wait/EOF watchdog expired"))}
            if let Some(fd)=&owner.stdout{if let Err(error)=Self::drain(fd.as_raw_fd(),&mut owner.out,&mut owner.out_eof){owner.fail(error)}}
            if let Some(fd)=&owner.stderr{if let Err(error)=Self::drain(fd.as_raw_fd(),&mut owner.err,&mut owner.err_eof){owner.fail(error)}}
            if owner.exit.is_none(){match owner.child.try_wait(){Ok(exit)=>owner.exit=exit,Err(error)=>owner.fail(error.into())}}
            if owner.failure.is_some()&&!owner.kill_sent{owner.kill_sent=true;let signal=if let Some(fd)=&owner.pidfd{
                if unsafe{libc::syscall(libc::SYS_pidfd_send_signal,fd.as_raw_fd(),libc::SIGKILL,std::ptr::null::<libc::siginfo_t>(),0)}==0{Ok(())}else{Err(io::Error::last_os_error())}
            }else{owner.child.kill()};if let Err(error)=signal{if error.raw_os_error()!=Some(libc::ESRCH){owner.fail(error.into())}}}
            if owner.exit.is_some()&&owner.out_eof&&owner.err_eof{break}
            // Unknown settlement retains SAME carrier/handles indefinitely.
            // Test watchdog never grants a product clock or absence fact.
            std::thread::sleep(Duration::from_millis(2));
        }
        let waited=loop{match owner.child.wait(){Ok(exit)=>break exit,Err(error)=>{owner.fail(error.into());std::thread::sleep(Duration::from_millis(2))}}};
        if let Some(error)=owner.failure.take(){return Err(error)}
        ensure!(owner.exit==Some(waited)&&waited.success(),"actual original test Child failed");
        let process=owner.process.as_ref().context("original child process identity absent")?;
        ensure!(process.pid==owner.child.id()&&process.exited()?&&owner.start.as_ref().is_some_and(|start|!start.is_empty()),"original Child/pidfd/start identity differs");Ok(owner)
    }
}
#[test]
#[ignore="requires initial-namespace UID0 with existing SETGID/SETUID/SETPCAP, NNP0; never skipped or granted"]
fn sdk_helper_root_created_stderr_pipe_survives_actual_uid1500_floor(){
    const TEST:&str="sdk_helper_root_created_stderr_pipe_survives_actual_uid1500_floor";
    if std::env::var(ROLE).as_deref()==Ok("probe"){probe().unwrap();return}
    assert!(std::env::var_os(ROLE).is_none(),"foreign fixture role");strict_root().unwrap();require_ignored_sigpipe();
    // Copied same ELF has genuinely traversable Root0755 ancestors. This
    // prevents /workspace0700 traversal from masquerading as procfd EACCES.
    let run=fs::symlink_metadata("/run").unwrap();assert!(run.is_dir()&&run.uid()==0&&run.mode()&0o022==0);
    let directory=PathBuf::from(format!("/run/rudder-sdk-helper-pipe-{}",uuid::Uuid::new_v4()));
    fs::DirBuilder::new().mode(0o755).create(&directory).unwrap();fs::set_permissions(&directory,fs::Permissions::from_mode(0o755)).unwrap();
    let path=directory.join("probe");let mut source=File::open(std::env::current_exe().unwrap()).unwrap();
    let mut target=OpenOptions::new().write(true).create_new(true).mode(0o555).open(&path).unwrap();io::copy(&mut source,&mut target).unwrap();target.sync_all().unwrap();drop(target);
    fs::set_permissions(&path,fs::Permissions::from_mode(0o555)).unwrap();Image::capture(&path).unwrap();
    let full=format!("{}::{TEST}",module_path!());let exact=full.split_once("::").unwrap().1;
    let mut command=Command::new(&path);command.args(["--exact",exact,"--ignored","--nocapture","--test-threads=1"])
        .env_clear().env(ROLE,"probe").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    unsafe{command.pre_exec(||sdk_helper_exec_floor());}
    let owner=OriginalChild::run(command).unwrap();assert!(owner.out_eof&&owner.err_eof);
    assert_eq!(owner.err,expected_frame(),"unchanged generic emitter must write nothing; fixed production emitter must write exactly one closed frame");
    assert_eq!(owner.out.split(|byte|*byte==b'\n').filter(|row|*row==CHILD_DONE.as_bytes()).count(),1);
    assert_eq!(bootstrap_failure_check(&super::super::handshake_diagnostic::failed_original_stderr(&owner.err,true)).as_deref(),Some("SdkHelperParentImage"));
    // Known actual wait + both EOFs precede disposable fixture cleanup and the
    // standalone PARENT completion marker. No fixture is SDK admission.
    fs::remove_file(path).unwrap();fs::remove_dir(directory).unwrap();println!("\n{ROOT_DONE}");
}
