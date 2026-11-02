//! Real kernel behavior of the SDK helper's workload user namespace. Each
//! scenario runs in a forked child, because setns into a user namespace
//! refuses a multithreaded caller, and reports back only its exit status.
use super::*;

fn pipe()->Result<(OwnedFd,OwnedFd)>{
    let mut fds=[-1;2];ensure!(unsafe{libc::pipe2(fds.as_mut_ptr(),libc::O_CLOEXEC)}==0,"test pipe unavailable");
    Ok(unsafe{(OwnedFd::from_raw_fd(fds[0]),OwnedFd::from_raw_fd(fds[1]))})
}
fn errno()->i32{io::Error::last_os_error().raw_os_error().unwrap_or(0)}
/// The helper only ever runs as UID/GID1500. Root drops to it in the child.
fn as_uid1500()->Result<()>{
    if unsafe{libc::geteuid()}==0{
        ensure!(unsafe{libc::setgroups(0,std::ptr::null())==0&&libc::setresgid(1500,1500,1500)==0&&libc::setresuid(1500,1500,1500)==0},"root test could not drop to UID1500");
    }
    ensure!(unsafe{libc::getuid()==1500&&libc::getgid()==1500},"run as actual UID1500 or genuine Root");Ok(())
}
fn in_forked_child(scenario:fn()->Result<()>){
    let pid=unsafe{libc::fork()};assert!(pid>=0,"test fork failed");
    if pid==0{
        let code=match std::panic::catch_unwind(||as_uid1500().and_then(|()|scenario())){
            Ok(Ok(()))=>0,
            Ok(Err(error))=>{let line=format!("{error:#}\n");unsafe{libc::write(2,line.as_ptr().cast(),line.len())};1}
            Err(_)=>2,
        };
        unsafe{libc::_exit(code)}
    }
    let mut status=0;assert_eq!(unsafe{libc::waitpid(pid,&mut status,0)},pid);
    assert!(libc::WIFEXITED(status)&&libc::WEXITSTATUS(status)==0,"scenario failed with status {status}");
}

/// The original helper made itself ND0 and then wrote its own maps. The
/// kernel refuses that open, which stalled every SDK qualification.
#[test]
fn nondumpable_process_cannot_write_its_own_uid_map(){
    in_forked_child(||{
        ensure!(unsafe{libc::prctl(libc::PR_SET_DUMPABLE,0,0,0,0)}==0,"ND0 failed");
        ensure!(unsafe{libc::unshare(libc::CLONE_NEWUSER)}==0,"unprivileged user namespaces unavailable");
        let error=OpenOptions::new().write(true).open("/proc/self/uid_map").err().context("ND0 uid_map open unexpectedly succeeded")?;
        ensure!(error.raw_os_error()==Some(libc::EACCES),"ND0 uid_map open failed with {error}, not EACCES");Ok(())
    });
}

#[test]
fn helper_joins_workload_namespace_without_becoming_dumpable(){
    in_forked_child(||{
        // A UID1500 process outside the namespace tries to take the helper's
        // stderr once the helper has joined.
        let(start_read,start_write)=pipe()?;let(result_read,result_write)=pipe()?;
        let helper=unsafe{libc::getpid()};
        let thief=unsafe{libc::fork()};ensure!(thief>=0,"thief fork failed");
        if thief==0{unsafe{
            let mut byte=0u8;if libc::read(start_read.as_raw_fd(),(&mut byte as *mut u8).cast(),1)!=1{libc::_exit(1)}
            let pidfd=libc::syscall(libc::SYS_pidfd_open,helper,0)as i32;
            let stolen=if pidfd<0{-1}else{libc::syscall(libc::SYS_pidfd_getfd,pidfd,2,0)as i32};
            let code:i32=if stolen>=0{0}else{*libc::__errno_location()};
            libc::write(result_write.as_raw_fd(),code.to_ne_bytes().as_ptr().cast(),4);libc::_exit(0)
        }}
        drop((start_read,result_write));
        ensure!(unsafe{libc::prctl(libc::PR_SET_DUMPABLE,0,0,0,0)}==0,"ND0 failed");
        let holder=WorkloadUserNamespace::fork()?;let child=holder.pid;
        holder.join(||Ok(Duration::from_secs(5)))?;
        ensure!(unsafe{libc::prctl(libc::PR_GET_DUMPABLE,0,0,0,0)}==0,"helper became dumpable");
        let mut status=0;ensure!(unsafe{libc::waitpid(child,&mut status,libc::WNOHANG)}==-1&&errno()==libc::ECHILD,"namespace child was not reaped");
        // The rest of the production sequence works from inside the namespace.
        ensure!(unsafe{libc::unshare(libc::CLONE_NEWNS|libc::CLONE_NEWNET|libc::CLONE_NEWPID)}==0,"remaining namespaces unavailable: errno {}",errno());
        mount(None,"/",None,(libc::MS_REC|libc::MS_PRIVATE)as _)?;
        ensure!(unsafe{libc::write(start_write.as_raw_fd(),b"1".as_ptr().cast(),1)}==1,"thief start failed");
        let mut code=[0u8;4];ensure!(unsafe{libc::read(result_read.as_raw_fd(),code.as_mut_ptr().cast(),4)}==4,"thief result absent");
        ensure!(unsafe{libc::waitpid(thief,&mut status,0)}==thief,"thief wait failed");
        ensure!(i32::from_ne_bytes(code)==libc::EPERM,"same-UID process took a helper descriptor (result {})",i32::from_ne_bytes(code));Ok(())
    });
}

#[test]
fn unused_workload_namespace_child_is_reaped_on_drop(){
    in_forked_child(||{
        ensure!(unsafe{libc::prctl(libc::PR_SET_DUMPABLE,0,0,0,0)}==0,"ND0 failed");
        let holder=WorkloadUserNamespace::fork()?;let child=holder.pid;drop(holder);
        let mut status=0;ensure!(unsafe{libc::waitpid(child,&mut status,libc::WNOHANG)}==-1&&errno()==libc::ECHILD,"namespace child was not reaped");
        ensure!(unsafe{libc::getuid()}==1500,"drop without join changed identity");Ok(())
    });
}
