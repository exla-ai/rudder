//! Fixed hidden qualifier entry. An arbitrary CLI invocation can only fail:
//! actual Root parent/peer, approved producer witness and sealed capsule are mandatory.
use super::*;
use std::os::unix::fs::PermissionsExt;
use std::{io::Read,os::unix::process::CommandExt};
pub(super) fn run()->Result<()> {
    let mut check=BootstrapCheck::SdkHelperEntry;
    let mut reaped_inner_stderr=None;
    let result=run_inner(&mut check,&mut reaped_inner_stderr).map_err(|error|bootstrap_failure_context(error,check));
    // Fixed bounded best-effort DATA on this ORIGINAL helper's stderr pipe.
    // Return the SAME error; neither logging nor a projection grants admission.
    if let Err(error)=&result{
        // Never place arbitrary tool bytes ahead of the trusted fixed frame.
        // A failed/unknown first write suppresses the secondary relay.
        if emit_helper_failure(error){if let Some(inner)=&reaped_inner_stderr{relay_inner_stderr_after_reap(inner);}}
    }
    result
}
fn run_inner(check:&mut BootstrapCheck,reaped_inner_stderr:&mut Option<File>)->Result<()> {
    let args:Vec<_>=std::env::args().collect();ensure!(args.len()==3&&args[1]==HELPER_ARG&&std::env::vars_os().next().is_none(),"SDK helper fixed entry differs");
    ensure!(unsafe{libc::getuid()}==1500&&unsafe{libc::geteuid()}==1500&&unsafe{libc::getgid()}==1500,"SDK helper requires actual unprivileged UID1500");
    ensure!(unsafe{libc::prctl(libc::PR_SET_DUMPABLE,0,0,0,0)}==0&&unsafe{libc::prctl(libc::PR_GET_DUMPABLE,0,0,0,0)}==0,"SDK helper ND0 unavailable");
    wire::require_initial_id_maps(std::process::id())?;
    // Forked before connect, so it never holds an SDK descriptor or capsule.
    let userns=WorkloadUserNamespace::fork()?;
    // Fresh connect only AFTER exec+ND0. No pre-exec inherited receiver socket
    // can be stolen and later receive this SDK descriptor capability.
    *check=BootstrapCheck::SdkHelperConnect;
    let raw=unsafe{libc::socket(libc::AF_UNIX,libc::SOCK_SEQPACKET|libc::SOCK_CLOEXEC|libc::SOCK_NONBLOCK,0)};
    ensure!(raw>=0,"SDK helper original connection unavailable");let socket=unsafe{OwnedFd::from_raw_fd(raw)};
    // Enable credentials BEFORE connect can let the original producer queue
    // its capsule. Endpoint capture keeps its existing same-socket recheck.
    let passcred:libc::c_int=1;ensure!(unsafe{libc::setsockopt(socket.as_raw_fd(),libc::SOL_SOCKET,libc::SO_PASSCRED,(&passcred as *const libc::c_int).cast(),std::mem::size_of_val(&passcred)as _)}==0,"SDK helper original credential reception unavailable");
    let(address,len)=abstract_address(&args[2])?;
    ensure!(unsafe{libc::connect(socket.as_raw_fd(),(&address as *const libc::sockaddr_un).cast(),len)}==0,"SDK helper original connect refused");
    *check=BootstrapCheck::SdkHelperParent;
    let peer=peer_pidfd(socket.as_raw_fd())?;let parent=Process::capture(peer)?;
    ensure!(parent.pid==unsafe{libc::getppid()}as u32,"SDK helper peer is not actual original parent");storage::floor(parent.pid,0)?;wire::require_initial_id_maps(parent.pid)?;
    *check=BootstrapCheck::SdkHelperParentImage;
    let service_image=Image::capture(Path::new(SERVICE_PATH))?;service_image.check()?;
    *check=BootstrapCheck::SdkHelperCapsule;
    let endpoint=RuntimeEndpoint::capture(socket)?;let mut packet=None;
    // No new enable budget: only wait for the original parent's one capsule.
    // Poll is bounded by parent's existing liveness; its cutoff is adopted
    // only from its actual sealed capsule, never a caller-provided CLI clock.
    while packet.is_none(){parent.live()?;packet=endpoint.receive_data()?;if packet.is_none(){std::thread::sleep(Duration::from_millis(1));}}
    let packet=packet.unwrap();let data=peer_witness::receive_capsule_data(&packet,&parent)?;
    *check=BootstrapCheck::SdkHelperServiceImage;
    peer_witness::validate_service_tuple_data(&data,&packet,&parent,&service_image)?;
    *check=BootstrapCheck::SdkHelperLauncher;
    // Association is the approved ORIGINAL Root producer assertion, not an
    // independent unprivileged proc-exe oracle for a hostile Root peer.
    *check=BootstrapCheck::SdkHelperLauncherImage;
    let launcher_image=Image::capture(Path::new(LAUNCHER_PATH))?;
    peer_witness::validate_launcher_tuple_data(&data,&packet,&parent,&launcher_image)?;
    *check=BootstrapCheck::SdkHelperFloor;
    let mount=&data.mount;mount.validate_data()?;
    ensure!(wire::fd_data(&packet.fds[5])?==mount.floor,"SDK helper held floor differs");
    let constraint=data.constraint.clone();let original=FinalizationBudget::from_constraint(&constraint)?;original.remaining()?;
    let start=original_stat(std::process::id())?.1;ensure!(data.helper_start_ticks==start,"SDK helper original start differs");
    *check=BootstrapCheck::SdkHelperWorkspace;
    let workspace=File::from(packet.fds[6].try_clone()?);let m=workspace.metadata()?;ensure!(m.is_dir()&&m.uid()==1500&&m.gid()==1500&&m.mode()&0o7777==0o700,"SDK helper private work directory differs");
    let cwd=fs::metadata(".")?;ensure!((cwd.dev(),cwd.ino())==(m.dev(),m.ino()),"SDK helper changed original work directory");
    let job=if data.job.is_null(){ensure!(packet.fds.len()==7,"SDK fixed descriptor count differs");None}else{ensure!(packet.fds.len()==10,"SDK selected descriptor count differs");Some((&data.job,&packet.fds[7],&packet.fds[8],&packet.fds[9]))};
    *check=BootstrapCheck::SdkHelperNamespaceWorkload;
    let workload=run_namespace_retaining(userns,&packet.fds[5],&packet.fds[6],&original,job,reaped_inner_stderr)?;if job.is_none(){validate_workload(&workload,&constraint)?;}
    let result=CompilerSdkProbeData{format_version:1,kind:"original-sdk-fixed-helper-complete-v1".into(),helper_pid:std::process::id(),helper_start_ticks:start,original_constraint:constraint,workload};
    *check=BootstrapCheck::SdkHelperResultSend;
    let bytes=canonical_json(&result)?;ensure!(bytes.len()<=16*1024,"SDK fixed result bound");
    loop{original.remaining()?;parent.live()?;if endpoint.send_data(&bytes,&[])?{break}std::thread::sleep(original.remaining()?.min(Duration::from_millis(1)));}Ok(())
}
fn mount(source:Option<&str>,target:&str,kind:Option<&str>,flags:libc::c_ulong)->Result<()> {
    let s=source.map(CString::new).transpose()?;let t=CString::new(target)?;let k=kind.map(CString::new).transpose()?;
    if unsafe{libc::mount(s.as_ref().map_or(std::ptr::null(),|v|v.as_ptr()),t.as_ptr(),k.as_ref().map_or(std::ptr::null(),|v|v.as_ptr()),flags,std::ptr::null())}!=0{
        return Err(io::Error::last_os_error()).context("SDK private namespace mount failed")
    }Ok(())
}
/// A retained descriptor still refers to its original mount after unshare.
/// A proc-FD bind source on that foreign mount fails EINVAL. Locate the same
/// inode through the new namespace, retaining the original as authority. The
/// reopened FD, rather than the pathname, is then the bind source.
fn rebase_mount_fd(original:&OwnedFd)->Result<OwnedFd>{
    let path=fs::read_link(format!("/proc/self/fd/{}",original.as_raw_fd()))?;
    rebase_mount_fd_at(original,&path)
}
fn rebase_mount_fd_at(original:&OwnedFd,path:&Path)->Result<OwnedFd>{
    ensure!(path.is_absolute()&&!path.as_os_str().as_encoded_bytes().ends_with(b" (deleted)"),"SDK retained mount locator unavailable");
    let file=File::from(original.try_clone()?);let before=file.metadata()?;
    ensure!(before.is_dir()||before.is_file(),"SDK retained mount role differs");
    let reopened=OpenOptions::new().read(true).custom_flags(libc::O_PATH|libc::O_NOFOLLOW|libc::O_CLOEXEC).open(path)
        .context("SDK retained mount reopen failed")?;
    let actual=reopened.metadata()?;let after=file.metadata()?;
    let identity=|m:&fs::Metadata|(m.dev(),m.ino(),m.mode()&libc::S_IFMT);
    ensure!(identity(&before)==identity(&actual)&&identity(&before)==identity(&after),"SDK retained mount inode differs");
    Ok(reopened.into())
}
/// The Root-private ancestors of the runtime floor cannot be traversed by
/// UID1500. Pin cwd to the held floor before cloning the mount namespace: the
/// kernel rebases cwd into the clone, without looking up those ancestors.
fn clone_mount_namespace_at(floor:&OwnedFd,workspace:&OwnedFd)->Result<OwnedFd>{
    if unsafe{libc::fchdir(floor.as_raw_fd())}!=0{return Err(io::Error::last_os_error()).context("SDK retained floor chdir failed")}
    if unsafe{libc::unshare(libc::CLONE_NEWNS|libc::CLONE_NEWNET|libc::CLONE_NEWPID)}!=0{return Err(io::Error::last_os_error()).context("SDK actual isolated namespaces unavailable")}
    mount(None,"/",None,(libc::MS_REC|libc::MS_PRIVATE)as _)?;
    let rebased:OwnedFd=OpenOptions::new().read(true).custom_flags(libc::O_PATH|libc::O_DIRECTORY|libc::O_CLOEXEC).open(".")?.into();
    let before=wire::fd_data(floor)?;let after=wire::fd_data(&rebased)?;
    ensure!((before.device,before.inode,before.directory)==(after.device,after.inode,after.directory),"SDK cloned floor inode differs");
    // The independently validated UID1500 scratch lives under /workspace;
    // reopening it still compares against its original held inode.
    let cwd=rebase_mount_fd(workspace)?;
    if unsafe{libc::fchdir(cwd.as_raw_fd())}!=0{return Err(io::Error::last_os_error()).context("SDK retained workspace chdir failed")}
    Ok(rebased)
}
/// Holds the workload user namespace for a helper that stays non-dumpable.
///
/// After unshare(CLONE_NEWUSER) a non-dumpable process keeps its proc files
/// owned by global root, so opening its own uid_map fails with EACCES. Making
/// the helper dumpable for the map writes would let any UID1500 process
/// pidfd_getfd the SDK descriptors it holds. Instead a child forked before the
/// helper connects (it holds no descriptor or capsule DATA) waits
/// non-dumpable, and only when asked turns dumpable, creates the namespace and
/// writes the maps. The helper joins it with setns and never stops being ND0.
pub(super) struct WorkloadUserNamespace{pid:libc::pid_t,pidfd:Option<OwnedFd>,go:Option<OwnedFd>,ready:Option<OwnedFd>}
impl WorkloadUserNamespace{
    /// Call while single-threaded: the child runs raw syscalls only.
    pub(super) fn fork()->Result<Self>{
        let pipe=||->Result<(OwnedFd,OwnedFd)>{let mut fds=[-1;2];ensure!(unsafe{libc::pipe2(fds.as_mut_ptr(),libc::O_CLOEXEC)}==0,"SDK user namespace pipe unavailable");Ok(unsafe{(OwnedFd::from_raw_fd(fds[0]),OwnedFd::from_raw_fd(fds[1]))})};
        let(go_read,go_write)=pipe()?;let(ready_read,ready_write)=pipe()?;
        let pid=unsafe{libc::fork()};ensure!(pid>=0,"SDK user namespace child unavailable");
        if pid==0{unsafe{user_namespace_child(go_read.as_raw_fd(),ready_write.as_raw_fd())}}
        drop((go_read,ready_write));
        let mut holder=Self{pid,pidfd:None,go:Some(go_write),ready:Some(ready_read)};
        // An unreaped child keeps its PID, so this pidfd names the same child.
        let raw=unsafe{libc::syscall(libc::SYS_pidfd_open,pid,0)}as i32;ensure!(raw>=0,"SDK user namespace child pidfd unavailable");
        holder.pidfd=Some(unsafe{OwnedFd::from_raw_fd(raw)});Ok(holder)
    }
    /// Joins the child's namespace as UID/GID0 mapped to the actual UID/GID1500.
    /// Dropping self afterwards kills and reaps the child.
    pub(super) fn join(self,remaining:impl Fn()->Result<Duration>)->Result<()>{
        let(Some(go),Some(ready),Some(pidfd))=(&self.go,&self.ready,&self.pidfd)else{anyhow::bail!("SDK user namespace child absent")};
        ensure!(unsafe{libc::write(go.as_raw_fd(),b"1".as_ptr().cast(),1)}==1,"SDK user namespace request failed");
        loop{
            let wait=remaining()?.as_millis().clamp(1,1000)as i32;let mut poll=libc::pollfd{fd:ready.as_raw_fd(),events:libc::POLLIN,revents:0};
            let n=unsafe{libc::poll(&mut poll,1,wait)};
            if n>0{break}
            ensure!(n==0||io::Error::last_os_error().kind()==io::ErrorKind::Interrupted,"SDK user namespace wait failed");
        }
        let mut byte=0u8;ensure!(unsafe{libc::read(ready.as_raw_fd(),(&mut byte as *mut u8).cast(),1)}==1&&byte==b'1',"SDK user namespace child failed");
        if unsafe{libc::setns(pidfd.as_raw_fd(),libc::CLONE_NEWUSER)}!=0{return Err(io::Error::last_os_error()).context("SDK user namespace join failed")}
        ensure!(unsafe{libc::getuid()==0&&libc::geteuid()==0&&libc::getgid()==0&&libc::getegid()==0},"SDK user namespace identity differs");
        for(path,expected)in[("/proc/self/uid_map","0 1500 1"),("/proc/self/gid_map","0 1500 1"),("/proc/self/setgroups","deny")]{
            ensure!(fs::read_to_string(path)?.split_whitespace().collect::<Vec<_>>().join(" ")==expected,"SDK user namespace map differs");
        }
        Ok(())
    }
}
impl Drop for WorkloadUserNamespace{
    fn drop(&mut self){
        self.go.take();self.ready.take();
        unsafe{libc::kill(self.pid,libc::SIGKILL);let mut status=0;while libc::waitpid(self.pid,&mut status,0)<0&&io::Error::last_os_error().kind()==io::ErrorKind::Interrupted{}}
        self.pidfd.take();
    }
}
/// Forked child only: raw syscalls, no allocation, never returns.
unsafe fn user_namespace_child(go:i32,ready:i32)->!{
    unsafe fn read_byte(fd:i32)->isize{let mut byte=0u8;loop{let n=libc::read(fd,(&mut byte as *mut u8).cast(),1);if n<0&&*libc::__errno_location()==libc::EINTR{continue}return n}}
    unsafe fn write_file(path:&std::ffi::CStr,data:&[u8])->bool{
        let fd=libc::open(path.as_ptr(),libc::O_WRONLY|libc::O_CLOEXEC);if fd<0{return false}
        let written=libc::write(fd,data.as_ptr().cast(),data.len())==data.len()as isize;libc::close(fd)==0&&written
    }
    let close=|from:i32,to:i32|from>to||libc::syscall(libc::SYS_close_range,from as u32,to as u32,0u32)==0;
    let(low,high)=(go.min(ready),go.max(ready));
    // Keep only the two pipes: no inherited stdio, so the Root parent's pipe
    // EOF never waits for this child.
    if !(close(0,low-1)&&close(low+1,high-1)&&libc::syscall(libc::SYS_close_range,(high+1)as u32,u32::MAX,0u32)==0){libc::_exit(1)}
    if read_byte(go)!=1{libc::_exit(1)}
    let created=libc::prctl(libc::PR_SET_DUMPABLE,1,0,0,0)==0&&libc::unshare(libc::CLONE_NEWUSER)==0
        &&write_file(c"/proc/self/setgroups",b"deny")&&write_file(c"/proc/self/uid_map",b"0 1500 1\n")&&write_file(c"/proc/self/gid_map",b"0 1500 1\n");
    if !created||libc::write(ready,b"1".as_ptr().cast(),1)!=1{libc::_exit(1)}
    // Keep the namespace alive until the helper has joined and closes go.
    while read_byte(go)>0{}
    libc::_exit(0)
}
fn run_namespace_retaining(userns:WorkloadUserNamespace,floor:&OwnedFd,workspace:&OwnedFd,original:&FinalizationBudget,job:Option<(&serde_json::Value,&OwnedFd,&OwnedFd,&OwnedFd)>,reaped_inner_stderr:&mut Option<File>)->Result<serde_json::Value> {
    original.remaining()?;
    userns.join(||original.remaining())?;
    let floor=clone_mount_namespace_at(floor,workspace)?;
    for name in ["root","workspace","tmp"]{fs::DirBuilder::new().mode(0o700).create(name)?;}
    if job.is_some(){fs::DirBuilder::new().mode(0o700).create("observer-proc")?;}
    mount(Some(&format!("/proc/self/fd/{}/prepared",floor.as_raw_fd())),"root",None,(libc::MS_BIND|libc::MS_REC)as _)?;
    mount(None,"root",None,(libc::MS_BIND|libc::MS_REMOUNT|libc::MS_RDONLY|libc::MS_NOSUID|libc::MS_NODEV)as _)?;
    let buck2=readonly_at(&floor,"buck2",false)?;
    OpenOptions::new().write(true).create_new(true).mode(0o500).open("workspace/buck2")?;
    mount(Some(&format!("/proc/self/fd/{}",buck2.as_raw_fd())),"workspace/buck2",None,libc::MS_BIND as _)?;
    mount(None,"workspace/buck2",None,(libc::MS_BIND|libc::MS_REMOUNT|libc::MS_RDONLY|libc::MS_NOSUID|libc::MS_NODEV)as _)?;
    if let Some((_,source,output,cancel))=job{check_cancel(cancel)?;
        let source=rebase_mount_fd(source)?;let output=rebase_mount_fd(output)?;
        for name in ["workspace/src","workspace/out"]{fs::DirBuilder::new().mode(0o700).create(name)?;}
        let rules:serde_json::Value=serde_json::from_str(include_str!("compiler_sdk_rules.json"))?;
        ensure!(rules["sandbox_profile"]=="uid1500-user-mount-net-pid-procless-seccomp-sdk-v3"&&rules["source_root"]=="/workspace/src"&&rules["output_root"]=="/workspace/out","SDK execution rule layout differs from actual isolated mounts");
        fs::write("workspace/rules.json",canonical_json(&rules)?)?;
        fs::set_permissions("workspace/rules.json",fs::Permissions::from_mode(0o444))?;
        mount(Some(&format!("/proc/self/fd/{}",source.as_raw_fd())),"workspace/src",None,(libc::MS_BIND|libc::MS_REC)as _)?;
        mount(None,"workspace/src",None,(libc::MS_BIND|libc::MS_REMOUNT|libc::MS_RDONLY|libc::MS_NOSUID|libc::MS_NODEV|libc::MS_NOEXEC)as _)?;
        mount(Some(&format!("/proc/self/fd/{}",output.as_raw_fd())),"workspace/out",None,libc::MS_BIND as _)?;
        mount(None,"workspace/out",None,(libc::MS_BIND|libc::MS_REMOUNT|libc::MS_NOSUID|libc::MS_NODEV|libc::MS_NOEXEC)as _)?;
    }
    // Recursion preserves the actual retained Source/output/Buck2 child
    // mounts. A nonrecursive bind would expose only their empty placeholders.
    mount(Some("workspace"),"root/workspace",None,(libc::MS_BIND|libc::MS_REC)as _)?;
    mount(Some("tmp"),"root/tmp",None,libc::MS_BIND as _)?;
    if job.is_some(){
        mount(None,"root/workspace",None,(libc::MS_BIND|libc::MS_REMOUNT|libc::MS_RDONLY|libc::MS_NOSUID|libc::MS_NODEV|libc::MS_NOEXEC)as _)?;
        mount(None,"root/tmp",None,(libc::MS_BIND|libc::MS_REMOUNT|libc::MS_NOSUID|libc::MS_NODEV|libc::MS_NOEXEC)as _)?;
        ensure!(fs::symlink_metadata("root/proc")?.is_dir()&&fs::read_dir("root/proc")?.next().is_none(),"selected SDK proc view must start empty");
    }
    // This reserved original FD prevents collision with Command's exec-error
    // pipe. Only the actual postfork PID1 child replaces it with its freshly
    // mounted proc root. No proc is mounted in the parent old PID namespace.
    let observer_proc_slot=if job.is_some(){Some(File::from(crate::root_storage_bootstrap::sealed_bytes(b"SDK observer proc reservation v2")?))}else{None};
    let observer_proc_fd=observer_proc_slot.as_ref().map(|file|file.as_raw_fd());
    // Fixed startup keeps its trusted proc layout; selected compilers see the
    // empty SDK proc directory and never inherit the observer-only descriptor.
    let output=OpenOptions::new().read(true).write(true).create_new(true).mode(0o600).open("workload.stdout")?;
    let error=OpenOptions::new().read(true).write(true).create_new(true).mode(0o600).open("workload.stderr")?;
    let mut command=Command::new("/tools/python/bin/python3");
    let selected_script;
    let script=if job.is_some(){
        selected_script=format!("{}\n{}",include_str!("compiler_sdk_confinement.py"),include_str!("compiler_sdk_selected.py"));
        selected_script.as_str()
    }else{include_str!("compiler_sdk_workload.py")};
    // Private bounded DATA to the observer only. It is never published in the
    // compiler filesystem, argv or environment, and is replaced by EOF on fd0
    // before compiler spawn. This does not create a Root/config capability.
    let observer_stdin=if let Some((request,_,_,_))=job{
        let bytes=canonical_json(request)?;ensure!(!bytes.is_empty()&&bytes.len()<=16*1024,"SDK observer request bound");
        Stdio::from(File::from(crate::root_storage_bootstrap::sealed_bytes(&bytes)?))
    }else{Stdio::null()};
    let deadline=original.observed_clock_bounds()?.0.to_string();
    let rules:serde_json::Value=serde_json::from_str(include_str!("compiler_sdk_rules.json"))?;
    if job.is_some(){command=Command::new(rules["python_executable"].as_str().context("SDK Python rule absent")?);for flag in rules["python_flags"].as_array().context("SDK Python flags absent")?{command.arg(flag.as_str().context("SDK Python flag invalid")?);}}
    else{command.args(["-s","-P"]);}
    command.args(["-c",script,&deadline]).env_clear().env("PYTHONHOME",if job.is_some(){rules["python_home"].as_str().context("SDK Python home absent")?}else{"/tools/python"})
        .stdin(observer_stdin).stdout(Stdio::from(output.try_clone()?)).stderr(Stdio::from(error.try_clone()?));
    if let Some(fd)=observer_proc_fd{command.arg(fd.to_string());}
    unsafe{command.pre_exec(move||{
        let target=if observer_proc_fd.is_some(){c"observer-proc"}else{c"root/proc"};
        if libc::mount(c"proc".as_ptr(),target.as_ptr(),c"proc".as_ptr(),(libc::MS_NOSUID|libc::MS_NODEV|libc::MS_NOEXEC)as _,std::ptr::null())!=0{return Err(io::Error::last_os_error())}
        if let Some(slot)=observer_proc_fd{
            let mut own=[0u8;32];let length=libc::readlink(c"observer-proc/self".as_ptr(),own.as_mut_ptr().cast(),own.len());
            if length!=1||own[0]!=b'1'{return Err(io::Error::other("SDK observer proc was not mounted in the actual fresh PID namespace"))}
            let opened=libc::open(c"observer-proc".as_ptr(),libc::O_PATH|libc::O_DIRECTORY|libc::O_NOFOLLOW|libc::O_CLOEXEC);
            if opened<0{return Err(io::Error::last_os_error())}
            if libc::dup2(opened,slot)<0{let error=io::Error::last_os_error();libc::close(opened);return Err(error)}
            if opened!=slot{libc::close(opened);}
            let flags=libc::fcntl(slot,libc::F_GETFD);
            if flags<0||libc::fcntl(slot,libc::F_SETFD,flags&!libc::FD_CLOEXEC)<0{return Err(io::Error::last_os_error())}
        }
        if libc::chroot(c"root".as_ptr())!=0||libc::chdir(c"/workspace".as_ptr())!=0||libc::prctl(libc::PR_SET_DUMPABLE,0,0,0,0)!=0{return Err(io::Error::last_os_error())}
        if libc::prctl(libc::PR_SET_NO_NEW_PRIVS,1,0,0,0)!=0||libc::prctl(libc::PR_SET_SECUREBITS,1|2|4|8,0,0,0)!=0{return Err(io::Error::last_os_error())}
        for cap in 0..64{if libc::prctl(libc::PR_CAPBSET_DROP,cap,0,0,0)!=0&&io::Error::last_os_error().raw_os_error()!=Some(libc::EINVAL){return Err(io::Error::last_os_error())}}
        #[repr(C)]struct Header{version:u32,pid:i32}#[repr(C)]#[derive(Clone,Copy)]struct Data{effective:u32,permitted:u32,inheritable:u32}
        let header=Header{version:0x20080522,pid:0};let data=[Data{effective:0,permitted:0,inheritable:0};2];
        if libc::syscall(libc::SYS_capset,&header as *const Header,data.as_ptr())!=0{return Err(io::Error::last_os_error())}Ok(())
    });}
    let mut child=command.spawn()?;
    let waited=(||->Result<()>{loop{original.remaining()?;if let Some((_,_,_,cancel))=job{check_cancel(cancel)?;}if let Some(exit)=child.try_wait()?{ensure!(exit.success(),"SDK actual isolated workload exit failed");return Ok(())}std::thread::sleep(original.remaining()?.min(Duration::from_millis(1)));}})();
    if let Err(failure)=waited{
        // This is the actual PID-namespace init Child, never a receipt/PID
        // lookup. Killing init fences its namespace descendants. Unknown
        // cleanup keeps this helper alive and Root retains its original child
        // pidfd; it cannot become an execution/normal-retirement ACK.
        let _=child.kill();loop{match child.try_wait(){Ok(Some(_))=>{
            // Transfer the SAME reaped workload stderr File to the outer helper.
            // Its trusted fixed frame must be attempted before any relay.
            *reaped_inner_stderr=Some(error);return Err(failure)
        },Ok(None)|Err(_)=>std::thread::sleep(Duration::from_millis(1))}}
    }
    let m=output.metadata()?;ensure!(m.len()>0&&m.len()<=16*1024&&error.metadata()?.len()<=64*1024,"SDK actual workload output bound");
    let mut raw=vec![0;m.len()as usize];ensure!(output.read_at(&mut raw,0)?==raw.len(),"SDK actual workload result incomplete");original.remaining()?;Ok(serde_json::from_slice(&raw)?)
}

/// Helper-only same inherited writer. Root established O_NONBLOCK before the
/// credential floor/exec; no proc reopen, DAC grant, OFD flag change or retry.
/// Logging is best effort and cannot create a result or settle a child.
pub(super) fn try_helper_diagnostic_write(fd:i32,bytes:&[u8])->bool {
    if bytes.is_empty()||bytes.len()>4096{return false}
    let mut metadata:libc::stat=unsafe{std::mem::zeroed()};
    if unsafe{libc::fstat(fd,&mut metadata)}!=0||metadata.st_mode&libc::S_IFMT!=libc::S_IFIFO{return false}
    let flags=unsafe{libc::fcntl(fd,libc::F_GETFL)};
    if flags<0||flags&libc::O_NONBLOCK==0||flags&libc::O_ACCMODE==libc::O_RDONLY{return false}
    let mut action:libc::sigaction=unsafe{std::mem::zeroed()};
    if unsafe{libc::sigaction(libc::SIGPIPE,std::ptr::null(),&mut action)}!=0||action.sa_sigaction!=libc::SIG_IGN{return false}
    unsafe{libc::write(fd,bytes.as_ptr().cast(),bytes.len())==bytes.len()as isize}
}
pub(super) fn emit_helper_failure(error:&anyhow::Error)->bool {
    let Ok(mut bytes)=serde_json::to_vec(&storage::retained_failure_data("compiler_sdk_helper","handshake",Some(error)))else{return false};
    if bytes.len()>=4096{return false}bytes.push(b'\n');
    try_helper_diagnostic_write(libc::STDERR_FILENO,&bytes)
}
const INNER_STDERR_HEX_PREFIX:&[u8]=b"SDK_INNER_STDERR_HEX_PREFIX_DATA (not execution evidence) ";
fn inner_stderr_relay_bytes(raw:&[u8])->Vec<u8> {
    const HEX:&[u8;16]=b"0123456789abcdef";
    let maximum=(4096-INNER_STDERR_HEX_PREFIX.len()-1)/2;
    let mut bytes=Vec::with_capacity(4096);bytes.extend_from_slice(INNER_STDERR_HEX_PREFIX);
    for byte in raw.iter().take(maximum){bytes.push(HEX[(byte>>4)as usize]);bytes.push(HEX[(byte&15)as usize]);}
    bytes.push(b'\n');bytes
}
/// Secondary private DATA only, after SAME actual workload Child reaped and
/// trusted fixed frame delivered. Exactly one bounded pread; raw line/JSON
/// injection is impossible because every observed byte is hex encoded.
fn relay_inner_stderr_after_reap(inner:&File) {
    let _=(||->Result<()> {
        let mut raw=[0u8;(4096-INNER_STDERR_HEX_PREFIX.len()-1)/2];
        let n=inner.read_at(&mut raw,0)?;if n==0{return Ok(())}
        let bytes=inner_stderr_relay_bytes(&raw[..n]);
        let _=try_helper_diagnostic_write(libc::STDERR_FILENO,&bytes);Ok(())
    })();
}
#[cfg(test)]
#[path="compiler_sdk_fixed_first_diagnostic_tests.rs"]
mod fixed_first_diagnostic_tests;
#[cfg(test)]
#[path="compiler_sdk_helper_userns_tests.rs"]
mod userns_tests;
