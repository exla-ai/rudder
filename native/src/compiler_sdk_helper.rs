//! Fixed hidden qualifier entry. An arbitrary CLI invocation can only fail:
//! actual global-Root parent/peer/proc-exe/sealed capsule are all mandatory.
use super::*;
use std::os::unix::fs::PermissionsExt;
use std::{io::Read,os::unix::process::CommandExt};
pub(super) fn run()->Result<()> {
    let args:Vec<_>=std::env::args().collect();ensure!(args.len()==3&&args[1]==HELPER_ARG&&std::env::vars_os().next().is_none(),"SDK helper fixed entry differs");
    ensure!(unsafe{libc::getuid()}==1500&&unsafe{libc::geteuid()}==1500&&unsafe{libc::getgid()}==1500,"SDK helper requires actual unprivileged UID1500");
    ensure!(unsafe{libc::prctl(libc::PR_SET_DUMPABLE,0,0,0,0)}==0&&unsafe{libc::prctl(libc::PR_GET_DUMPABLE,0,0,0,0)}==0,"SDK helper ND0 unavailable");
    wire::require_initial_id_maps(std::process::id())?;
    // Fresh connect only AFTER exec+ND0. No pre-exec inherited receiver socket
    // can be stolen and later receive this SDK descriptor capability.
    let raw=unsafe{libc::socket(libc::AF_UNIX,libc::SOCK_SEQPACKET|libc::SOCK_CLOEXEC|libc::SOCK_NONBLOCK,0)};
    ensure!(raw>=0,"SDK helper original connection unavailable");let socket=unsafe{OwnedFd::from_raw_fd(raw)};
    let(address,len)=abstract_address(&args[2])?;
    ensure!(unsafe{libc::connect(socket.as_raw_fd(),(&address as *const libc::sockaddr_un).cast(),len)}==0,"SDK helper original connect refused");
    let peer=peer_pidfd(socket.as_raw_fd())?;let parent=Process::capture(peer)?;
    ensure!(parent.pid==unsafe{libc::getppid()}as u32,"SDK helper peer is not actual original parent");storage::floor(parent.pid,0)?;wire::require_initial_id_maps(parent.pid)?;
    Image::capture(Path::new(SERVICE_PATH))?.matches_process(&parent)?;
    let endpoint=RuntimeEndpoint::capture(socket)?;let mut packet=None;
    // No new enable budget: only wait for the original parent's one capsule.
    // Poll is bounded by parent's existing liveness; its cutoff is adopted
    // only from its actual sealed capsule, never a caller-provided CLI clock.
    while packet.is_none(){parent.live()?;packet=endpoint.receive_data()?;if packet.is_none(){std::thread::sleep(Duration::from_millis(1));}}
    let packet=packet.unwrap();ensure!(packet.credentials.pid==parent.pid as i32&&packet.credentials.uid==0&&packet.credentials.gid==0&&[7,10].contains(&packet.fds.len()),"SDK helper capsule origin/FD set differs");
    let raw=super::super::super::read_sealed_bytes(&packet.fds[0],16*1024)?;
    ensure!(raw==packet.bytes,"SDK helper sealed original bytes differ");let data:serde_json::Value=serde_json::from_slice(&raw)?;
    ensure!(data["version"]==1&&data["kind"]=="root-compiler-sdk-fixed-helper-v1"&&data["service_pid"]==parent.pid&&data["helper_pid"]==std::process::id(),"SDK helper original identities differ");
    let service=Process::capture(packet.fds[1].try_clone()?)?;ensure!(service.pid==parent.pid,"SDK helper original service pidfd differs");
    Image::capture(Path::new(SERVICE_PATH))?.matches_fd(&packet.fds[2])?;
    let launcher=Process::capture(packet.fds[3].try_clone()?)?;ensure!(original_stat(parent.pid)?.0==launcher.pid&&data["launcher_pid"]==launcher.pid,"SDK helper original launcher parent differs");
    wire::require_initial_id_maps(launcher.pid)?;storage::floor(launcher.pid,0)?;
    let launcher_image=Image::capture(Path::new(LAUNCHER_PATH))?;launcher_image.matches_fd(&packet.fds[4])?;launcher_image.matches_process(&launcher)?;
    let mount:CompilerSdkMountData=serde_json::from_value(data["mount"].clone())?;mount.validate_data()?;
    ensure!(wire::fd_data(&packet.fds[5])?==mount.floor,"SDK helper held floor differs");
    let constraint:FinalizationConstraint=serde_json::from_value(data["constraint"].clone())?;let original=FinalizationBudget::from_constraint(&constraint)?;original.remaining()?;
    let start=original_stat(std::process::id())?.1;ensure!(data["helper_start_ticks"]==start,"SDK helper original start differs");
    let workspace=File::from(packet.fds[6].try_clone()?);let m=workspace.metadata()?;ensure!(m.is_dir()&&m.uid()==1500&&m.gid()==1500&&m.mode()&0o7777==0o700,"SDK helper private work directory differs");
    let cwd=fs::metadata(".")?;ensure!((cwd.dev(),cwd.ino())==(m.dev(),m.ino()),"SDK helper changed original work directory");
    let job=if data["job"].is_null(){ensure!(packet.fds.len()==7,"SDK fixed descriptor count differs");None}else{ensure!(packet.fds.len()==10,"SDK selected descriptor count differs");Some((&data["job"],&packet.fds[7],&packet.fds[8],&packet.fds[9]))};
    let workload=run_namespace_retaining(&packet.fds[5],&original,job)?;if job.is_none(){validate_workload(&workload,&constraint)?;}
    let result=CompilerSdkProbeData{format_version:1,kind:"original-sdk-fixed-helper-complete-v1".into(),helper_pid:std::process::id(),helper_start_ticks:start,original_constraint:constraint,workload};
    let bytes=canonical_json(&result)?;ensure!(bytes.len()<=16*1024,"SDK fixed result bound");
    loop{original.remaining()?;parent.live()?;if endpoint.send_data(&bytes,&[])?{break}std::thread::sleep(original.remaining()?.min(Duration::from_millis(1)));}Ok(())
}
fn mount(source:Option<&str>,target:&str,kind:Option<&str>,flags:libc::c_ulong)->Result<()> {
    let s=source.map(CString::new).transpose()?;let t=CString::new(target)?;let k=kind.map(CString::new).transpose()?;
    ensure!(unsafe{libc::mount(s.as_ref().map_or(std::ptr::null(),|v|v.as_ptr()),t.as_ptr(),k.as_ref().map_or(std::ptr::null(),|v|v.as_ptr()),flags,std::ptr::null())}==0,"SDK private namespace mount failed");Ok(())
}
fn run_namespace_retaining(floor:&OwnedFd,original:&FinalizationBudget,job:Option<(&serde_json::Value,&OwnedFd,&OwnedFd,&OwnedFd)>)->Result<serde_json::Value> {
    original.remaining()?;
    ensure!(unsafe{libc::unshare(libc::CLONE_NEWUSER|libc::CLONE_NEWNS|libc::CLONE_NEWNET|libc::CLONE_NEWPID)}==0,"SDK actual isolated namespaces unavailable");
    fs::write("/proc/self/setgroups",b"deny")?;fs::write("/proc/self/uid_map",b"0 1500 1\n")?;fs::write("/proc/self/gid_map",b"0 1500 1\n")?;
    mount(None,"/",None,(libc::MS_REC|libc::MS_PRIVATE)as _)?;
    for name in ["root","workspace","tmp"]{fs::DirBuilder::new().mode(0o700).create(name)?;}
    if job.is_some(){fs::DirBuilder::new().mode(0o700).create("observer-proc")?;}
    mount(Some(&format!("/proc/self/fd/{}/prepared",floor.as_raw_fd())),"root",None,(libc::MS_BIND|libc::MS_REC)as _)?;
    mount(None,"root",None,(libc::MS_BIND|libc::MS_REMOUNT|libc::MS_RDONLY|libc::MS_NOSUID|libc::MS_NODEV)as _)?;
    let buck2=readonly_at(floor,"buck2",false)?;
    OpenOptions::new().write(true).create_new(true).mode(0o500).open("workspace/buck2")?;
    mount(Some(&format!("/proc/self/fd/{}",buck2.as_raw_fd())),"workspace/buck2",None,libc::MS_BIND as _)?;
    mount(None,"workspace/buck2",None,(libc::MS_BIND|libc::MS_REMOUNT|libc::MS_RDONLY|libc::MS_NOSUID|libc::MS_NODEV)as _)?;
    if let Some((_,source,output,cancel))=job{check_cancel(cancel)?;
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
            relay_inner_stderr_after_reap(&error);return Err(failure)
        },Ok(None)|Err(_)=>std::thread::sleep(Duration::from_millis(1))}}
    }
    let m=output.metadata()?;ensure!(m.len()>0&&m.len()<=16*1024&&error.metadata()?.len()<=64*1024,"SDK actual workload output bound");
    let mut raw=vec![0;m.len()as usize];ensure!(output.read_at(&mut raw,0)?==raw.len(),"SDK actual workload result incomplete");original.remaining()?;Ok(serde_json::from_slice(&raw)?)
}

/// Secondary private DATA only, called after the SAME actual Child was reaped.
/// One bounded pread and the existing SIGPIPE-safe nonblocking single writer;
/// no retry, new clock, compiler-status claim or change to the original error.
fn relay_inner_stderr_after_reap(inner:&File) {
    let _=(||->Result<()> {
        const PREFIX:&[u8]=b"SDK_INNER_STDERR_PREFIX_DATA (not execution evidence)\n";
        let mut bytes=[0u8;4096];bytes[..PREFIX.len()].copy_from_slice(PREFIX);
        let n=inner.read_at(&mut bytes[PREFIX.len()..],0)?;
        if n==0{return Ok(())}
        let raw=unsafe{libc::fcntl(libc::STDERR_FILENO,libc::F_DUPFD_CLOEXEC,3)};
        if raw<0{return Ok(())}
        let output=unsafe{OwnedFd::from_raw_fd(raw)};
        let _=crate::root_storage_bootstrap::try_observation_write(&output,&bytes[..PREFIX.len()+n]);
        Ok(())
    })();
}
