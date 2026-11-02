//! Strict, ignored real-exec boundary regressions, NOT SDK qualification.
//! No host execution here. Negative and reverse-positive gates are separate.
use super::*;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
const ROLE:&str="RUDDER_SDK_EXE_BOUNDARY_ROLE";
const PARENT_FD:&str="RUDDER_SDK_EXE_BOUNDARY_PARENT_FD";
const DENIED:&str="RUDDER_SDK_HELPER_CHILD_ROOT_EXE_DENIED_V1=passed";
const PARENT_DONE:&str="RUDDER_SDK_HELPER_EXE_BOUNDARY_PARENT_CHILD_V1=passed";
fn exact_command(path:&Path,test:&str,role:&str)->Command{
    let full=format!("{}::{test}",module_path!());let exact=full.split_once("::").unwrap().1;
    let mut command=Command::new(path);
    command.args(["--exact",exact,"--ignored","--nocapture","--test-threads=1"])
        .env_clear().env(ROLE,role).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());command
}
fn strict_root(reverse:bool)->Result<()>{
    storage::floor(std::process::id(),0)?;wire::require_initial_id_maps(std::process::id())?;
    let status=String::from_utf8(storage::proc_bytes("/proc/thread-self/status",16384)?)?;
    let field=|name:&str|->Result<u64>{Ok(u64::from_str_radix(status.lines().find_map(|line|line.strip_prefix(name)).context("strict Root capability field absent")?.trim(),16)?)};
    let required=(1u64<<6)|(1u64<<7)|(1u64<<8);
    ensure!(field("CapEff:")?&required==required,"strict actual Root lacks SETGID/SETUID/SETPCAP");
    ensure!(field("CapBnd:")?!=0,"strict actual Root bounding floor is already empty");
    ensure!(status.lines().any(|line|line.strip_prefix("NoNewPrivs:").is_some_and(|value|value.trim()=="0")),"strict actual Root must start NNP0");
    if reverse{ensure!(field("CapEff:")?&(1u64<<19)!=0,"strict reverse-positive requires existing actual SYS_PTRACE; never granted or skipped");}Ok(())
}
struct OriginalChild {
    child:Child,stdout:Option<std::process::ChildStdout>,stderr:Option<std::process::ChildStderr>,
    pidfd:Option<OwnedFd>,process:Option<Process>,start:Option<String>,out:Vec<u8>,err:Vec<u8>,out_eof:bool,err_eof:bool,
    exit:Option<ExitStatus>,failure:Option<anyhow::Error>,kill_sent:bool,may_kill_original:bool,
}
impl OriginalChild {
    fn drain(fd:i32,raw:&mut Vec<u8>,eof:&mut bool)->Result<()>{
        if *eof{return Ok(())}let mut poll=libc::pollfd{fd,events:libc::POLLIN,revents:0};
        let ready=unsafe{libc::poll(&mut poll,1,0)};
        if ready<0{let error=io::Error::last_os_error();if error.kind()==io::ErrorKind::Interrupted{return Ok(())}return Err(error.into())}
        ensure!(poll.revents&(libc::POLLERR|libc::POLLNVAL)==0,"strict original pipe poll failed");
        if ready==0{return Ok(())}let mut bytes=[0u8;4096];let n=unsafe{libc::read(fd,bytes.as_mut_ptr().cast(),bytes.len())};
        if n<0{let error=io::Error::last_os_error();if error.kind()==io::ErrorKind::Interrupted{return Ok(())}return Err(error.into())}
        if n==0{*eof=true;return Ok(())}
        let room=8192usize.saturating_sub(raw.len());raw.extend_from_slice(&bytes[..(n as usize).min(room)]);
        ensure!(n as usize<=room,"strict original pipe bound exceeded");Ok(())
    }
    fn fail(&mut self,error:anyhow::Error){if self.failure.is_none(){self.failure=Some(error);}}
    fn signal_failed_original(&mut self){
        if self.failure.is_none()||self.kill_sent||!self.may_kill_original{return}self.kill_sent=true;
        let result=if let Some(process)=&self.process{if unsafe{libc::syscall(libc::SYS_pidfd_send_signal,process.fd().as_raw_fd(),libc::SIGKILL,std::ptr::null::<libc::siginfo_t>(),0)}==0{Ok(())}else{Err(io::Error::last_os_error())}}else{self.child.kill()};
        if let Err(error)=result{if error.raw_os_error()!=Some(libc::ESRCH){self.fail(error.into())}}
    }
    fn run(mut command:Command,may_kill_original:bool,mut step:impl FnMut(&mut Self)->Result<()>)->Result<Self>{
        // Store the SAME actual Child before every fallible pipe/pidfd/start
        // operation. On error/timeout keep it until actual wait AND both EOFs.
        let started=Instant::now();let child=command.spawn()?;
        let mut owner=Self{child,stdout:None,stderr:None,pidfd:None,process:None,start:None,out:Vec::new(),err:Vec::new(),out_eof:false,err_eof:false,exit:None,failure:None,kill_sent:false,may_kill_original};
        owner.stdout=owner.child.stdout.take();owner.stderr=owner.child.stderr.take();
        let captured=(||->Result<()>{
            ensure!(owner.stdout.is_some()&&owner.stderr.is_some(),"strict original pipe custody absent");
            let raw=unsafe{libc::syscall(libc::SYS_pidfd_open,owner.child.id(),0)}as i32;
            if raw<0{return Err(io::Error::last_os_error().into())}
            owner.pidfd=Some(unsafe{OwnedFd::from_raw_fd(raw)});owner.process=Some(Process::capture(owner.pidfd.as_ref().unwrap().try_clone()?)?);owner.start=Some(original_stat(owner.child.id())?.1);Ok(())
        })();if let Err(error)=captured{owner.fail(error)}
        loop{
            if started.elapsed()>=Duration::from_secs(20){owner.fail(anyhow::anyhow!("strict original child wait/EOF watchdog expired"));}
            if let Some(fd)=&owner.stdout{if let Err(error)=Self::drain(fd.as_raw_fd(),&mut owner.out,&mut owner.out_eof){owner.fail(error)}}
            if let Some(fd)=&owner.stderr{if let Err(error)=Self::drain(fd.as_raw_fd(),&mut owner.err,&mut owner.err_eof){owner.fail(error)}}
            if owner.exit.is_none(){match owner.child.try_wait(){Ok(exit)=>owner.exit=exit,Err(error)=>owner.fail(error.into())}}
            if owner.failure.is_none(){if let Err(error)=step(&mut owner){owner.fail(error)}}
            owner.signal_failed_original();
            if owner.exit.is_some()&&owner.out_eof&&owner.err_eof{break}
            // Expiration never creates another work/acceptance clock. Remain
            // with the SAME originals if actual settlement cannot be observed.
            std::thread::sleep(Duration::from_millis(2));
        }
        let waited=loop{match owner.child.wait(){Ok(exit)=>break exit,Err(error)=>{owner.fail(error.into());std::thread::sleep(Duration::from_millis(2));}}};ensure!(owner.exit==Some(waited),"strict original actual wait changed");
        if let Some(error)=owner.failure.take(){return Err(error)}
        let process=owner.process.as_ref().context("strict original pidfd absent")?;
        ensure!(process.pid==owner.child.id()&&process.exited()?&&owner.start.as_ref().is_some_and(|start|!start.is_empty()),"strict original birth/pidfd/Child differs");
        ensure!(waited.success()&&owner.out_eof&&owner.err_eof,"strict original Child failed or EOF absent");Ok(owner)
    }
}
fn probe()->Result<()>{
    storage::floor(std::process::id(),1500)?;wire::require_initial_id_maps(std::process::id())?;
    if unsafe{libc::prctl(libc::PR_SET_DUMPABLE,0,0,0,0)}!=0{return Err(io::Error::last_os_error().into())}
    require_protected_descriptor_directory(std::process::id())?;
    let raw:i32=std::env::var(PARENT_FD)?.parse()?;let parent=Process::capture(unsafe{OwnedFd::from_raw_fd(raw)})?;
    ensure!(parent.pid==unsafe{libc::getppid()}as u32,"strict actual original Root parent differs");storage::floor(parent.pid,0)?;
    // The actual copied immutable ELF has Root-controlled traversable 0755
    // ancestors and 0555 mode. These succeed before exercising proc/exe.
    let image=Image::capture(&std::env::current_exe()?)?;image.check()?;parent.live()?;
    let error=image.matches_process(&parent).expect_err("real zero-cap UID1500 must not read Root parent's proc exe");
    let os=error.chain().find_map(|cause|cause.downcast_ref::<io::Error>()).context("actual proc-exe denial lost IO error")?;
    ensure!(os.kind()==io::ErrorKind::PermissionDenied&&os.raw_os_error()==Some(libc::EACCES),"actual cross-credential proc-exe denial differs");
    println!("\n{DENIED}");std::io::stdout().flush()?;
    let mut release=[0u8;1];std::io::stdin().read_exact(&mut release)?;ensure!(release==[1],"strict original parent release differs");Ok(())
}
fn pair(test:&str,reverse:bool)->Result<()>{
    strict_root(reverse)?;let image=Image::capture(&std::env::current_exe()?)?;let parent=Process::current()?;image.matches_process(&parent)?;
    let inherited=parent.fd().as_raw_fd();let mut command=exact_command(&std::env::current_exe()?,test,"probe");command.env(PARENT_FD,inherited.to_string());
    unsafe{command.pre_exec(move||{let flags=libc::fcntl(inherited,libc::F_GETFD);if flags<0||libc::fcntl(inherited,libc::F_SETFD,flags&!libc::FD_CLOEXEC)<0{return Err(io::Error::last_os_error())}sdk_helper_exec_floor()});}
    let mut released=false;
    let owner=OriginalChild::run(command,true,|owner|{
        if !released&&owner.out.split(|byte|*byte==b'\n').any(|line|line==DENIED.as_bytes()){
            let process=owner.process.as_ref().context("strict actual child pidfd absent")?;storage::floor(process.pid,1500)?;require_protected_descriptor_directory(process.pid)?;
            ensure!(original_stat(process.pid)?.1==*owner.start.as_ref().unwrap(),"strict original child start changed");
            if reverse{image.matches_process(process)?;}
            owner.child.stdin.as_mut().context("strict original release pipe absent")?.write_all(&[1])?;released=true;
        }Ok(())
    })?;
    ensure!(released&&owner.out.split(|byte|*byte==b'\n').filter(|line|*line==DENIED.as_bytes()).count()==1,"strict exact negative boundary child did not execute");
    println!("\n{PARENT_DONE}");Ok(())
}
fn boundary(test:&str,reverse:bool,marker:&str){
    match std::env::var(ROLE).as_deref(){
        Ok("probe")=>{probe().unwrap();return},Ok("parent")=>{pair(test,reverse).unwrap();return},Err(_)=>{},_=>panic!("strict boundary role differs"),
    }
    strict_root(reverse).unwrap();
    // Run the genuine Root parent from the SAME safe immutable image as its
    // zero-cap child. No inaccessible /workspace700 ancestor can fake EACCES.
    let directory=PathBuf::from(format!("/run/rudder-sdk-exe-boundary-{}",uuid::Uuid::new_v4()));
    fs::DirBuilder::new().mode(0o755).create(&directory).unwrap();fs::set_permissions(&directory,fs::Permissions::from_mode(0o755)).unwrap();let path=directory.join("probe");
    let mut source=File::open(std::env::current_exe().unwrap()).unwrap();
    let mut target=OpenOptions::new().write(true).create_new(true).mode(0o555).open(&path).unwrap();io::copy(&mut source,&mut target).unwrap();target.sync_all().unwrap();drop(target);
    fs::set_permissions(&path,fs::Permissions::from_mode(0o555)).unwrap();Image::capture(&path).unwrap();
    // The outer owner NEVER kills this Root parent on timeout/error: that
    // parent owns the actual nested probe. Only its inner owner may signal
    // the probe; retain the Root carrier until actual wait AND both EOFs.
    let owner=OriginalChild::run(exact_command(&path,test,"parent"),false,|_|Ok(())).unwrap();
    assert_eq!(owner.out.split(|byte|*byte==b'\n').filter(|line|*line==PARENT_DONE.as_bytes()).count(),1,"actual original Root parent did not validate child");
    println!("\n{marker}");
}
#[test]
#[ignore="requires genuine initial-namespace UID0 plus existing SETGID/SETUID/SETPCAP; never skipped or forged"]
fn sdk_helper_zero_cap_child_cannot_read_original_root_parent_exe(){
    boundary("sdk_helper_zero_cap_child_cannot_read_original_root_parent_exe",false,"RUDDER_SDK_HELPER_ROOT_EXE_DENIAL_V1=passed");
}
#[test]
#[ignore="requires genuine initial-namespace UID0 and existing SYS_PTRACE plus SETGID/SETUID/SETPCAP; never grants capabilities"]
fn sdk_helper_existing_root_ptrace_reads_original_nd0_child_exe(){
    boundary("sdk_helper_existing_root_ptrace_reads_original_nd0_child_exe",true,"RUDDER_SDK_HELPER_ROOT_PTRACE_REVERSE_V1=passed");
}
