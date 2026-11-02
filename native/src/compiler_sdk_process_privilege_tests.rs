//! Real exec isolation: credential/dumpable changes stay in disposable children.
//! The strict positive path requires genuine global UID0 and capability bits;
//! it does not skip, spoof credentials or mint any SDK owner/result.
use super::*;

const ROLE: &str = "RUDDER_SDK_PROCESS_FLOOR_TEST_ROLE";
const PROBE_DONE: &str = "RUDDER_SDK_PROCESS_UNPRIVILEGED_PROBE_V1=passed";
const FLOOR_DONE: &str = "RUDDER_SDK_HELPER_ROOT_EXEC_FLOOR_CHILD_V1=passed";
const ROOT_DONE: &str = "RUDDER_SDK_HELPER_ROOT_EXEC_FLOOR_V1=passed";

fn status_field<'a>(status: &'a str, field: &str) -> &'a str {
    status.lines().find_map(|line|line.strip_prefix(field)).unwrap().trim()
}
fn current_status() -> String {
    String::from_utf8(storage::proc_bytes("/proc/thread-self/status",16384).unwrap()).unwrap()
}
fn exact_command(test: &str, role: &str) -> Command {
    let mut command=Command::new(std::env::current_exe().unwrap());
    let full=format!("{}::{test}",module_path!());
    let exact=full.split_once("::").unwrap().1;
    command.args(["--exact",exact,"--include-ignored","--nocapture","--test-threads=1"])
        .env_clear().env(ROLE,role).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    command
}
fn run_original_child(mut command:Command,marker:&str) {
    fn drain(fd:i32,bytes:&mut Vec<u8>,eof:&mut bool)->Result<()> {
        if *eof{return Ok(())}
        let mut chunk=[0u8;4096];
        for _ in 0..16 {
            let n=unsafe{libc::read(fd,chunk.as_mut_ptr().cast(),chunk.len())};
            if n==0{*eof=true;break}
            if n<0 {
                let error=io::Error::last_os_error();
                if error.kind()==io::ErrorKind::WouldBlock{break}
                if error.kind()==io::ErrorKind::Interrupted{continue}
                return Err(error.into())
            }
            ensure!(bytes.len()+n as usize<=8192,"regression pipe bound exceeded");
            bytes.extend_from_slice(&chunk[..n as usize]);
        }
        Ok(())
    }
    // One test-only watchdog covers the SAME actual child wait and BOTH EOFs.
    // It never changes product finalization/source/SDK clocks.
    let started=std::time::Instant::now();
    let mut child=command.spawn().expect("original regression child spawn failed");
    let stdout=child.stdout.take().unwrap();let stderr=child.stderr.take().unwrap();
    let(mut out,mut err)=(Vec::new(),Vec::new());
    let(mut out_eof,mut err_eof,mut actual)=(false,false,None);
    let result=(||->Result<()> {
        for fd in [stdout.as_raw_fd(),stderr.as_raw_fd()] {
            let flags=unsafe{libc::fcntl(fd,libc::F_GETFL)};
            ensure!(flags>=0&&unsafe{libc::fcntl(fd,libc::F_SETFL,flags|libc::O_NONBLOCK)}==0,"regression pipe nonblocking failed");
        }
        loop {
            ensure!(started.elapsed()<Duration::from_secs(20),"original regression child wait/EOF watchdog expired");
            if actual.is_none(){actual=child.try_wait()?;}
            drain(stdout.as_raw_fd(),&mut out,&mut out_eof)?;
            drain(stderr.as_raw_fd(),&mut err,&mut err_eof)?;
            if actual.is_some()&&out_eof&&err_eof{return Ok(())}
            std::thread::sleep(Duration::from_millis(2));
        }
    })();
    if result.is_err(){let _=child.kill();let _=child.wait();}
    result.unwrap();
    let out=String::from_utf8(out).unwrap();let err=String::from_utf8(err).unwrap();
    assert!(actual.unwrap().success(),"actual regression child failed; stdout={out}; stderr={err}");
    assert!(out_eof&&err_eof,"both actual original pipe EOFs are mandatory");
    assert!(out.lines().any(|line|line.ends_with(marker)),"actual exact child test did not execute: {out}");
}

#[test]
fn sdk_helper_unprivileged_exec_refuses_root_transition_and_checks_actual_nd0() {
    const TEST:&str="sdk_helper_unprivileged_exec_refuses_root_transition_and_checks_actual_nd0";
    if std::env::var(ROLE).as_deref()==Ok("unprivileged") {
        assert_eq!(unsafe{libc::getuid()},1500);assert_eq!(unsafe{libc::geteuid()},1500);
        assert_eq!(unsafe{libc::getgid()},1500);assert_eq!(unsafe{libc::getegid()},1500);
        // A UID1500 caller cannot run the production Root-only transition.
        // spawn must return the actual pre_exec errno, not an executed child.
        let mut refused=Command::new(std::env::current_exe().unwrap());
        refused.args(["--list"]).env_clear().stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
        unsafe{refused.pre_exec(||sdk_helper_exec_floor());}
        let error=match refused.spawn(){
            Err(error)=>error,
            Ok(mut unexpected)=>{let _=unexpected.kill();let _=unexpected.wait();panic!("unprivileged actor executed the Root-only transition")}
        };
        assert_eq!(error.raw_os_error(),Some(libc::EPERM));
        // Match the real helper: exec happens first; ND0 is reapplied before
        // its fresh connect. The old /proc/PID uid0 test rejects this floor.
        let pid=std::process::id();
        for dumpable in [0,1,0] {
            assert_eq!(unsafe{libc::prctl(libc::PR_SET_DUMPABLE,dumpable,0,0,0)},0);
            assert_eq!(unsafe{libc::prctl(libc::PR_GET_DUMPABLE,0,0,0,0)},dumpable);
            let process=fs::symlink_metadata(format!("/proc/{pid}")).unwrap();
            assert_eq!(process.uid(),1500);assert_eq!(process.mode()&0o7777,0o555);
            let result=require_protected_descriptor_directory(pid);
            assert_eq!(result.is_ok(),dumpable==0,"actual ND0 descriptor guard differed");
        }
        println!("{PROBE_DONE}");return
    }
    let mut command=exact_command(TEST,"unprivileged");
    if unsafe{libc::geteuid()}==0 {
        // Credential changes happen only in the forked child, with the exact
        // production ordering. An unprivileged test harness needs no UID trick.
        unsafe{command.pre_exec(||sdk_helper_exec_floor());}
    }else{
        assert_eq!(unsafe{libc::getuid()},1500,"run local probe as actual UID1500 or genuine Root");
    }
    run_original_child(command,PROBE_DONE);
}

#[test]
#[ignore="requires actual global UID0 with effective CAP_SETGID/CAP_SETUID/CAP_SETPCAP and original NNP0"]
fn sdk_helper_exec_floor_drops_root_privileges_before_actual_exec() {
    const TEST:&str="sdk_helper_exec_floor_drops_root_privileges_before_actual_exec";
    if std::env::var(ROLE).as_deref()==Ok("root-floor") {
        // This reads the post-exec kernel status, before any test credential
        // mutation. It checks all four UID/GID values, all five cap sets,
        // empty supplementary groups and NNP1 via the actual product guard.
        storage::floor(std::process::id(),1500).unwrap();
        assert_eq!(unsafe{libc::prctl(libc::PR_GET_KEEPCAPS,0,0,0,0)},0);
        assert_eq!(unsafe{libc::setresuid(0,0,0)},-1);
        assert_eq!(io::Error::last_os_error().raw_os_error(),Some(libc::EPERM));
        assert_eq!(unsafe{libc::setresgid(0,0,0)},-1);
        assert_eq!(io::Error::last_os_error().raw_os_error(),Some(libc::EPERM));
        assert_eq!(unsafe{libc::prctl(libc::PR_SET_DUMPABLE,0,0,0,0)},0);
        require_protected_descriptor_directory(std::process::id()).unwrap();
        assert_eq!(fs::symlink_metadata(format!("/proc/{}",std::process::id())).unwrap().uid(),1500);
        println!("{FLOOR_DONE}");return
    }
    storage::floor(std::process::id(),0).expect("strict regression requires actual all-zero UID/GID IDs");
    wire::require_initial_id_maps(std::process::id()).expect("strict regression requires initial user namespace mappings");
    let status=current_status();
    let effective=u64::from_str_radix(status_field(&status,"CapEff:"),16).unwrap();
    let required=(1u64<<6)|(1u64<<7)|(1u64<<8); // SETGID, SETUID, SETPCAP only
    assert_eq!(effective&required,required,"strict original Root lacks required effective capability bits");
    assert_ne!(status_field(&status,"CapBnd:"),"0000000000000000","strict regression must remove actual bounding bits");
    assert_eq!(status_field(&status,"NoNewPrivs:"),"0","strict original Root must start at NNP0");
    // std applies Command UID/GID setters BEFORE pre_exec. Preserve this real
    // baseline reproduction: clearing groups there must fail before exec.
    let mut obsolete=Command::new(std::env::current_exe().unwrap());
    obsolete.args(["--list"]).env_clear().stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).uid(1500).gid(1500);
    unsafe{obsolete.pre_exec(||{
        if libc::setgroups(0,std::ptr::null())!=0{return Err(io::Error::last_os_error())}Ok(())
    });}
    match obsolete.spawn(){
        Err(error)=>assert_eq!(error.raw_os_error(),Some(libc::EPERM)),
        Ok(mut unexpected)=>{let _=unexpected.kill();let _=unexpected.wait();panic!("obsolete UID/GID ordering unexpectedly reached exec")}
    }
    let mut command=exact_command(TEST,"root-floor");
    unsafe{command.pre_exec(||sdk_helper_exec_floor());}
    run_original_child(command,FLOOR_DONE);
    // All changes happened in forked children, never the Root test harness.
    assert_eq!(status_field(&current_status(),"NoNewPrivs:"),"0");
    assert_eq!(status_field(&current_status(),"CapEff:"),status_field(&status,"CapEff:"));
    assert_eq!(status_field(&current_status(),"CapBnd:"),status_field(&status,"CapBnd:"));
    // Only the actual Root parent can emit this completion, after real child
    // exit, both original EOFs, child marker and unchanged-parent checks.
    println!("\n{ROOT_DONE}");
}
