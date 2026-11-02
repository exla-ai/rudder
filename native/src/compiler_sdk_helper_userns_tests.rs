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
fn in_forked_child(scenario:impl FnOnce()->Result<()>+std::panic::UnwindSafe){
    in_forked_child_after(as_uid1500,scenario)
}
fn in_forked_child_after(floor:fn()->Result<()>,scenario:impl FnOnce()->Result<()>+std::panic::UnwindSafe){
    let pid=unsafe{libc::fork()};assert!(pid>=0,"test fork failed");
    if pid==0{
        let code=match std::panic::catch_unwind(||floor().and_then(|()|scenario())){
            Ok(Ok(()))=>0,
            Ok(Err(error))=>{let line=format!("{error:#}\n");unsafe{libc::write(2,line.as_ptr().cast(),line.len())};1}
            Err(_)=>2,
        };
        unsafe{libc::_exit(code)}
    }
    let mut status=0;assert_eq!(unsafe{libc::waitpid(pid,&mut status,0)},pid);
    assert!(libc::WIFEXITED(status)&&libc::WEXITSTATUS(status)==0,"scenario failed with status {status}");
}

struct TestDirectory(PathBuf);
impl TestDirectory{
    fn new()->Self{
        let path=PathBuf::from(format!("/workspace/rudder-sdk-namespace-test-{}",uuid::Uuid::new_v4()));fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        if unsafe{libc::geteuid()}==0{let name=CString::new(path.as_os_str().as_encoded_bytes()).unwrap();assert_eq!(unsafe{libc::chown(name.as_ptr(),1500,1500)},0);}
        Self(path)
    }
}
impl Drop for TestDirectory{fn drop(&mut self){let _=fs::remove_dir_all(&self.0);}}

#[test]
fn helper_rebases_original_file_and_directory_before_isolated_bind(){
    let fixture=TestDirectory::new();
    fs::create_dir(fixture.0.join("source")).unwrap();fs::write(fixture.0.join("source/member"),b"original").unwrap();
    fs::create_dir(fixture.0.join("directory-target")).unwrap();fs::write(fixture.0.join("file-target"),b"").unwrap();
    let path=fixture.0.clone();
    let(directory,file)=fixture_mount_descriptors(&path.join("source"));
    in_forked_child(move||exercise_held_mount_rebase(&path,directory,file));
    assert_eq!(fs::read(fixture.0.join("source/member")).unwrap(),b"original");
}
fn fixture_mount_descriptors(source:&Path)->(OwnedFd,OwnedFd){
    let directory:OwnedFd=OpenOptions::new().read(true).custom_flags(libc::O_PATH|libc::O_DIRECTORY|libc::O_NOFOLLOW).open(source).unwrap().into();
    let file=readonly_at(&directory,"member",false).unwrap();(directory,file)
}
fn exercise_held_mount_rebase(path:&Path,directory:OwnedFd,file:OwnedFd)->Result<()>{
        let workspace:OwnedFd=OpenOptions::new().read(true).custom_flags(libc::O_PATH|libc::O_DIRECTORY|libc::O_NOFOLLOW).open(path)?.into();
        ensure!(unsafe{libc::fchdir(workspace.as_raw_fd())}==0,"test workspace chdir failed");
        ensure!(unsafe{libc::prctl(libc::PR_SET_DUMPABLE,0,0,0,0)}==0,"ND0 failed");
        WorkloadUserNamespace::fork()?.join(||Ok(Duration::from_secs(5)))?;
        let cloned_directory=clone_mount_namespace_at(&directory,&workspace)?;
        let target=path.join("directory-target");let target=target.to_str().context("test target encoding")?;
        let failure=mount(Some(&format!("/proc/self/fd/{}",directory.as_raw_fd())),target,None,(libc::MS_BIND|libc::MS_REC)as _).expect_err("foreign retained mount unexpectedly accepted");
        ensure!(failure.downcast_ref::<io::Error>().is_some_and(|error|error.raw_os_error()==Some(libc::EINVAL)),"foreign mount errno was not preserved");
        for(original,target,recursive)in[(&directory,path.join("directory-target"),libc::MS_REC),(&file,path.join("file-target"),0)]{
            let rebased=if recursive!=0{cloned_directory.try_clone()?}else{readonly_at(&cloned_directory,"member",false)?};
            let before=wire::fd_data(original)?;let after=wire::fd_data(&rebased)?;
            ensure!((before.device,before.inode,before.directory)==(after.device,after.inode,after.directory)&&before.mount_id!=after.mount_id,"rebase lost original inode or retained foreign mount");
            let target=target.to_str().context("test target encoding")?;
            mount(Some(&format!("/proc/self/fd/{}",rebased.as_raw_fd())),target,None,(libc::MS_BIND|recursive)as _)?;
            mount(None,target,None,(libc::MS_BIND|libc::MS_REMOUNT|libc::MS_RDONLY|libc::MS_NOSUID|libc::MS_NODEV)as _)?;
        }
        ensure!(fs::read(path.join("directory-target/member"))?==b"original"&&fs::read(path.join("file-target"))?==b"original","bound original content differs");
        ensure!(fs::write(path.join("directory-target/member"),b"changed").err().is_some_and(|e|e.raw_os_error()==Some(libc::EROFS))
            &&fs::write(path.join("file-target"),b"changed").err().is_some_and(|e|e.raw_os_error()==Some(libc::EROFS)),"readonly bind allowed a write");
        ensure!(unsafe{libc::prctl(libc::PR_GET_DUMPABLE,0,0,0,0)}==0,"rebase changed ND0");Ok(())
}

/// Requires the genuine global Root precursor. This variant proves the mount
/// rebase after the same irreversible capability/identity floor as production;
/// it does not claim an exec or inherited stderr-pipe qualification.
#[test]
#[ignore = "requires genuine global Root with bounding capabilities and no prior NNP"]
fn root_floor_rebases_retained_descriptors_without_relaxing_nd0(){
    let pid=unsafe{libc::getpid()}as u32;
    storage::floor(pid,0).unwrap();wire::require_initial_id_maps(pid).unwrap();
    let status=fs::read_to_string("/proc/thread-self/status").unwrap();
    let field=|name:&str|status.lines().find_map(|line|line.strip_prefix(name)).unwrap().trim();
    let effective=u64::from_str_radix(field("CapEff:"),16).unwrap();
    assert_eq!(effective&((1<<6)|(1<<7)|(1<<8)),(1<<6)|(1<<7)|(1<<8));
    assert_ne!(u64::from_str_radix(field("CapBnd:"),16).unwrap(),0);
    assert_eq!(field("NoNewPrivs:"),"0");
    fn original_privilege_floor()->Result<()>{unsafe{
        if libc::syscall(libc::SYS_setgroups,0usize,std::ptr::null::<libc::gid_t>())!=0{return Err(io::Error::last_os_error().into())}
        crate::root_storage_bootstrap::restrict_bounding(0)?;
        if libc::prctl(libc::PR_SET_KEEPCAPS,0,0,0,0)!=0
            ||libc::prctl(libc::PR_CAP_AMBIENT,libc::PR_CAP_AMBIENT_CLEAR_ALL,0,0,0)!=0
            ||libc::prctl(libc::PR_SET_NO_NEW_PRIVS,1,0,0,0)!=0
            ||libc::syscall(libc::SYS_setresgid,1500u32,1500u32,1500u32)!=0
            ||libc::syscall(libc::SYS_setresuid,1500u32,1500u32,1500u32)!=0{return Err(io::Error::last_os_error().into())}
        crate::root_storage_bootstrap::clear_capabilities()?;
        storage::floor(libc::getpid()as u32,1500)?;Ok(())
    }}
    let fixture=TestDirectory::new();
    let private=fixture.0.join("root-private");fs::DirBuilder::new().mode(0o700).create(&private).unwrap();
    let source=private.join("source");fs::create_dir(&source).unwrap();fs::write(source.join("member"),b"original").unwrap();
    fs::create_dir(fixture.0.join("directory-target")).unwrap();fs::write(fixture.0.join("file-target"),b"").unwrap();
    let(directory,file)=fixture_mount_descriptors(&source);
    let path=fixture.0.clone();
    in_forked_child_after(original_privilege_floor,move||{
        let error=OpenOptions::new().read(true).custom_flags(libc::O_PATH).open(&source).err().context("Root-private ancestor was traversable")?;
        ensure!(error.raw_os_error()==Some(libc::EACCES),"private ancestor refusal errno differs");
        exercise_held_mount_rebase(&path,directory,file)
    });
    assert_eq!(fs::read(private.join("source/member")).unwrap(),b"original");
    println!("RUDDER_SDK_HELPER_ROOT_MOUNT_REBASE_V1=passed");
}

#[test]
fn retained_mount_rebase_refuses_replaced_deleted_symlink_and_unreachable_locators(){
    let fixture=TestDirectory::new();let path=fixture.0.clone();
    in_forked_child(move||{
        let name=path.join("original");let moved=path.join("moved");fs::write(&name,b"original")?;
        let original:OwnedFd=OpenOptions::new().read(true).custom_flags(libc::O_PATH|libc::O_NOFOLLOW).open(&name)?.into();
        // Simulate replacement after readlink located the original but before
        // reopen. Neither a different inode nor a symlink adopts authority.
        fs::rename(&name,&moved)?;fs::write(&name,b"replacement")?;
        ensure!(rebase_mount_fd_at(&original,&name).is_err(),"replacement inode accepted");
        fs::remove_file(&name)?;std::os::unix::fs::symlink(&moved,&name)?;
        ensure!(rebase_mount_fd_at(&original,&name).is_err(),"symlink locator accepted");
        ensure!(rebase_mount_fd_at(&original,Path::new("(unreachable)/original")).is_err(),"unreachable locator accepted");
        fs::remove_file(&moved)?;ensure!(rebase_mount_fd(&original).is_err(),"deleted original accepted");
        let directory=path.join("directory");fs::create_dir(&directory)?;
        let held:OwnedFd=OpenOptions::new().read(true).custom_flags(libc::O_PATH|libc::O_DIRECTORY).open(&directory)?.into();
        fs::rename(&directory,path.join("moved-directory"))?;fs::create_dir(&directory)?;
        ensure!(rebase_mount_fd_at(&held,&directory).is_err(),"replacement directory accepted");Ok(())
    });
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
