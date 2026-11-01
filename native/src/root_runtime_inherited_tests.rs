//! Isolated actual descriptor-table regressions. These use harmless real FDs
//! and constructor helpers, never a fake Root/bootstrap/registration ACK.
use super::*;
use std::{io::Write,os::unix::{process::CommandExt,fs::FileTypeExt,net::UnixStream}};
const MODE:&str="RUDDER_RUNTIME_ENTRY_FD_TEST_MODE";
const CHILD:&str="root_storage_bootstrap::runtime_image::inherited_tests::entry_descriptor_child";
fn parent(mode:&str,slots:&[i32]){
    // Core3/4 and optional5..12 are controlled ONLY in the new child. Parent
    // process/test-harness descriptors are never closed or reclassified.
    // Reserve low Parent numbers with CLOEXEC owners so std::Command's own
    // exec-error pipe cannot collide with the child's fixed3..12 mapping.
    let reservation=File::open("/dev/null").unwrap();let mut low=Vec::new();
    loop{let fd=unsafe{libc::fcntl(reservation.as_raw_fd(),libc::F_DUPFD_CLOEXEC,3)};assert!(fd>=3);low.push(unsafe{OwnedFd::from_raw_fd(fd)});if fd>=32{break}}
    let (release_read,mut release_write)=UnixStream::pair().unwrap();
    let mut keep=Vec::new();let mut mapping=Vec::new();
    for target in [3,4].into_iter().chain(slots.iter().copied()){
        let file=File::open("/dev/null").unwrap();let from=if target==3{release_read.as_raw_fd()}else{file.as_raw_fd()};
        let raw=unsafe{libc::fcntl(from,libc::F_DUPFD_CLOEXEC,64)};assert!(raw>=64);
        keep.push(unsafe{OwnedFd::from_raw_fd(raw)});mapping.push((raw,target));
    }
    let mut command=std::process::Command::new(std::env::current_exe().unwrap());command.args(["--exact",CHILD,"--ignored","--nocapture","--test-threads=1"])
        .env_clear().env(MODE,mode).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    unsafe{command.pre_exec(move||{
        for fd in 3..=12{let flags=libc::fcntl(fd,libc::F_GETFD);if flags>=0&&libc::fcntl(fd,libc::F_SETFD,flags|libc::FD_CLOEXEC)!=0{return Err(io::Error::last_os_error())}}
        for(from,to)in &mapping{if libc::dup2(*from,*to)<0{return Err(io::Error::last_os_error())}}
        Ok(())
    });}
    let original=FinalizationBudget::begin(Duration::from_secs(30)).unwrap();let mut child=command.spawn().unwrap();drop(command);drop(keep);drop(low);drop(reservation);drop(release_read);
    let mut original_pidfd=None;
    let result=(||->Result<()>{let raw=unsafe{libc::syscall(libc::SYS_pidfd_open,child.id(),0)}as i32;ensure!(raw>=0,"fixture original child pidfd unavailable");original_pidfd=Some(unsafe{OwnedFd::from_raw_fd(raw)});
        release_write.write_all(b"G")?;
        loop{original.remaining()?;if let Some(status)=child.try_wait()?{ensure!(status.success(),"isolated descriptor fixture failed");return Ok(())}
            std::thread::sleep(original.phase_limit(Duration::from_millis(2))?);}})();
    if result.is_err(){let _=child.kill();let _=child.wait();}result.unwrap();
    let mut exited=libc::pollfd{fd:original_pidfd.as_ref().unwrap().as_raw_fd(),events:libc::POLLIN,revents:0};assert_eq!(unsafe{libc::poll(&mut exited,1,0)},1);
}
#[test]
fn absent_entry_stays_absent_after_actual_constructor_fd_allocations(){parent("absent",&[])}
#[test]
fn partial_entry_owns_only_originals_and_refuses_complete_transfer(){parent("partial",&[5,8,12])}
#[test]
fn complete_entry_moves_exact_originals_once_without_double_ownership(){parent("complete",&[5,6,7,8,9,10,11,12])}
#[test]
#[ignore="isolated child fixture only; controlled FD table supplied by three parent tests"]
fn entry_descriptor_child(){
    // First fixture operation mirrors the fixed service entry: no File open,
    // process/namespace capture, proc read or constructor before this snapshot.
    let mut captured=unsafe{CapturedRuntimeDescriptors::take_at_service_entry()};
    let core=[unsafe{OwnedFd::from_raw_fd(3)},unsafe{OwnedFd::from_raw_fd(4)}];
    // Parent retains the actual Child/pidfd BEFORE this fixture may finish;
    // raw read allocates no descriptor into the optional slots.
    let mut release=[0u8;1];assert_eq!(unsafe{libc::read(3,release.as_mut_ptr().cast(),1)},1);assert_eq!(&release,b"G");
    let mode=std::env::var(MODE).expect("isolated entry fixture routing absent");
    match mode.as_str(){
        "absent"=>{
            assert!(captured.is_absent_at_entry());captured.validate_shape().unwrap();
            // Real helpers used by RootStorageService's constructor allocate
            // into ALL optional numeric slots after they were absent at entry.
            let process=Process::current().unwrap();let mnt=storage::namespace(None,"mnt",libc::CLONE_NEWNS).unwrap();let user=storage::namespace(None,"user",libc::CLONE_NEWUSER).unwrap();
            let mut internal=vec![mnt,user];let mut fd_numbers=vec![process.fd().as_raw_fd()];
            while fd_numbers.iter().copied().chain(internal.iter().map(|f|f.as_raw_fd())).filter(|n|(5..=12).contains(n)).count()<8{
                internal.push(File::open("/dev/null").unwrap());assert!(internal.len()<32,"isolated constructor allocator did not reach optional slots");}
            fd_numbers.extend(internal.iter().map(|f|f.as_raw_fd()));for raw in 5..=12{assert!(fd_numbers.contains(&raw));}
            let mut runtime=None;RootRuntimeService::capture_entry_snapshot_retaining(&mut runtime,&mut captured,&process).unwrap();assert!(runtime.is_none());
            drop(captured);for raw in &fd_numbers{assert!(unsafe{libc::fcntl(*raw,libc::F_GETFD)}>=0,"entry absence stole an internal owner");}
            // Even an accidental second snapshot cannot adopt the allocations.
            let second=unsafe{CapturedRuntimeDescriptors::take_at_service_entry()};assert!(second.validate_shape().is_err());assert!(second.raw.iter().all(Option::is_none));drop(second);
            for raw in &fd_numbers{assert!(unsafe{libc::fcntl(*raw,libc::F_GETFD)}>=0);}
        },
        "partial"=>{
            assert!(!captured.is_absent_at_entry());assert!(captured.validate_shape().is_err());let mut destination=Vec::new();assert!(captured.take_complete_into_retaining(&mut destination).is_err());assert!(destination.is_empty());
            for(index,slot)in captured.raw.iter().enumerate(){let expected=[5,8,12].contains(&(5+index as i32));assert_eq!(slot.is_some(),expected);if let Some(fd)=slot{assert_eq!(fd.as_raw_fd(),5+index as i32);assert!(unsafe{libc::fcntl(fd.as_raw_fd(),libc::F_GETFD)}&libc::FD_CLOEXEC!=0);}}
            let internal=File::open("/dev/null").unwrap();let number=internal.as_raw_fd();assert!(![5,8,12].contains(&number));drop(captured);assert!(unsafe{libc::fcntl(number,libc::F_GETFD)}>=0);
            for raw in [5,8,12]{assert_eq!(unsafe{libc::fcntl(raw,libc::F_GETFD)},-1);assert_eq!(io::Error::last_os_error().raw_os_error(),Some(libc::EBADF));}
        },
        "complete"=>{
            captured.validate_shape().unwrap();let process=Process::current().unwrap();let mut runtime=None;
            // A complete FD TABLE is not authority: harmless /dev/null inputs
            // fail the actual endpoint/proof validators, AFTER originals are
            // staged in the real service carrier. The failed owner keeps them.
            assert!(RootRuntimeService::capture_entry_snapshot_retaining(&mut runtime,&mut captured,&process).is_err());
            let actual=runtime.as_ref().unwrap();assert_eq!(actual.raw.len(),8);assert!(actual.proof.is_none());assert!(captured.raw.iter().all(Option::is_none));
            let mut second=Vec::new();assert!(captured.take_complete_into_retaining(&mut second).is_err());assert!(second.is_empty());drop(captured);
            for(index,fd)in actual.raw.iter().enumerate(){assert_eq!(fd.as_raw_fd(),5+index as i32);assert!(unsafe{libc::fcntl(fd.as_raw_fd(),libc::F_GETFD)}&libc::FD_CLOEXEC!=0);assert!(File::from(fd.try_clone().unwrap()).metadata().unwrap().file_type().is_char_device());}
            runtime.as_mut().unwrap().close_admission();drop(runtime);for raw in 5..=12{assert_eq!(unsafe{libc::fcntl(raw,libc::F_GETFD)},-1);assert_eq!(io::Error::last_os_error().raw_os_error(),Some(libc::EBADF));}
            // Reusing one now-free number remains owned by its NEW opener.
            let replacement=File::open("/dev/null").unwrap();assert!((5..=12).contains(&replacement.as_raw_fd()));assert!(unsafe{libc::fcntl(replacement.as_raw_fd(),libc::F_GETFD)}>=0);
        },
        _=>panic!("unknown descriptor fixture mode"),
    }
    drop(core);
}
