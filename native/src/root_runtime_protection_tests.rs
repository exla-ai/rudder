//! Real Linux proc ownership regression in an isolated exec, so dumpable and
//! credential changes never affect the parent test harness or sibling tests.
use super::*;
use std::io::Read;

const ROLE: &str = "RUDDER_PROTECTED_PROC_CHILD";
const COMPLETED: &str = "RUDDER_PROTECTED_PROC_CHILD_COMPLETED";

#[test]
fn protected_fd_directory_accepts_nondumpable_engine_and_refuses_dumpable_peer() {
    if std::env::var_os(ROLE).as_deref() == Some(std::ffi::OsStr::new("1")) {
        assert_ne!(unsafe { libc::geteuid() }, 0);
        let pid = std::process::id();
        for dumpable in [0, 1, 0] {
            assert_eq!(
                unsafe { libc::prctl(libc::PR_SET_DUMPABLE, dumpable, 0, 0, 0) },
                0
            );
            assert_eq!(
                unsafe { libc::prctl(libc::PR_GET_DUMPABLE, 0, 0, 0, 0) },
                dumpable
            );
            let process = fs::symlink_metadata(format!("/proc/{pid}")).unwrap();
            // The former predicate rejects a correctly protected engine: the
            // kernel deliberately preserves effective ownership on 0555 dirs.
            assert_eq!(process.mode() & 0o7777, 0o555);
            assert_eq!(process.uid(), unsafe { libc::geteuid() });
            assert_ne!(process.uid(), 0);
            let fd = fs::symlink_metadata(format!("/proc/{pid}/fd")).unwrap();
            assert!(fd.is_dir());
            assert_eq!(fd.mode() & 0o7777, 0o500);
            assert_eq!(
                fd.uid(),
                if dumpable == 0 {
                    0
                } else {
                    unsafe { libc::geteuid() }
                }
            );
            assert_eq!(
                fd.gid(),
                if dumpable == 0 {
                    0
                } else {
                    unsafe { libc::getegid() }
                }
            );
            let result = proof_check(BootstrapCheck::EngineProtectedProc, || {
                require_protected_descriptor_directory(pid)
            });
            if dumpable == 0 {
                result.unwrap();
            } else {
                assert_eq!(
                    bootstrap_failure_check(&result.unwrap_err()).as_deref(),
                    Some("EngineProtectedProc")
                );
            }
        }
        println!("{COMPLETED}");
        return;
    }

    let test = format!(
        "{}::protected_fd_directory_accepts_nondumpable_engine_and_refuses_dumpable_peer",
        module_path!()
    );
    // module_path includes the crate name; libtest's exact filter starts at the
    // first module. The child executes this same test after a real exec.
    let test = test.split_once("::").unwrap().1;
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .env_clear()
        .env(ROLE, "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped());
    // Root CI uses an unprivileged child too. All transition syscalls happen in
    // Command's child, and the test reapplies/verifies ND0 after exec.
    if unsafe { libc::geteuid() } == 0 {
        unsafe {
            command.pre_exec(|| {
                if libc::setgroups(0, std::ptr::null()) != 0
                    || libc::setresgid(1500, 1500, 1500) != 0
                    || libc::setresuid(1500, 1500, 1500) != 0
                {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let original = FinalizationBudget::begin(Duration::from_secs(30)).unwrap();
    let mut child = command.spawn().unwrap();
    let result = (|| -> Result<()> {
        loop {
            original.remaining()?;
            if let Some(status) = child.try_wait()? {
                ensure!(
                    status.success(),
                    "isolated protected proc regression failed"
                );
                return Ok(());
            }
            std::thread::sleep(original.phase_limit(Duration::from_millis(2))?);
        }
    })();
    if result.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    result.unwrap();
    let mut output = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .take(4096)
        .read_to_string(&mut output)
        .unwrap();
    assert!(
        output.lines().any(|line| line.ends_with(COMPLETED)),
        "child exact test did not execute the real proc checks"
    );
}
