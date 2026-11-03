//! Real selected Python/TypeScript execution, using the existing helper API.
//! The UID1500 test supervisor substitutes for the Root parent ONLY at the
//! private namespace-carrier boundary. No helper entry, capsule, SDK catalogue,
//! result admission or runtime-retirement permission is forged by this test.
use super::*;

const MAIN: &str = "export const answer: number = 42;\n";
const JAVASCRIPT: &[u8] = b"export const answer = 42;\n";

struct RealFixture(PathBuf);
impl RealFixture {
    fn new() -> Self {
        let root = PathBuf::from(format!("/workspace/rudder-sdk-real-{}", uuid::Uuid::new_v4()));
        let fixture = Self(root.clone());
        let prepare = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/sdk_local_e2e/prepare.py");
        let output = Command::new("/usr/bin/python3").args(["-I", "-B"]).arg(prepare)
            .arg("--prepare").arg(&root).output().expect("install pinned /usr/bin/python3 for the offline SDK fixture");
        assert!(output.status.success(), "real SDK fixture precondition failed (no SDK execution claimed): {}",
            String::from_utf8_lossy(&output.stderr));
        fixture.verify_closure();
        println!("LOCAL_SDK_REAL_FIXTURE={}", String::from_utf8_lossy(&output.stdout).trim());
        fixture
    }

    fn verify_closure(&self) {
        let manifest: serde_json::Value = serde_json::from_slice(&fs::read(self.0.join("closure.json")).unwrap()).unwrap();
        assert_eq!(manifest["sdk_admission"], false);
        let expected: serde_json::Value = serde_json::from_str(include_str!("pins.json")).unwrap();
        assert_eq!(manifest["principal_pins"], expected);
        for member in manifest["members"].as_array().unwrap() {
            let relative = Path::new(member["path"].as_str().unwrap());
            assert!(relative.components().all(|part| matches!(part, std::path::Component::Normal(_))));
            let bytes = fs::read(self.0.join("floor").join(relative)).unwrap();
            assert_eq!(member["size"], bytes.len() as u64);
            assert_eq!(member["sha256"], digest(&bytes), "local SDK closure changed: {}", relative.display());
        }
    }
}
impl Drop for RealFixture {
    fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Case { Compile, CompilerError, DeniedProc, DeniedHostInput, CancelBeforeStart, CancelRunning, Deadline }
impl Case {
    fn name(self) -> &'static str {
        match self {
            Self::Compile => "compile", Self::CompilerError => "compiler-error",
            Self::DeniedProc => "denied-proc", Self::DeniedHostInput => "denied-host-input",
            Self::CancelBeforeStart => "cancel-before-start", Self::CancelRunning => "cancel-running",
            Self::Deadline => "original-deadline",
        }
    }
}

fn eventfd() -> OwnedFd {
    let raw = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
    assert!(raw >= 0, "local original cancellation eventfd unavailable");
    unsafe { OwnedFd::from_raw_fd(raw) }
}
fn cancel(fd: &OwnedFd) {
    let one = 1u64;
    assert_eq!(unsafe { libc::write(fd.as_raw_fd(), (&one as *const u64).cast(), 8) }, 8);
}

fn source_access_watch(path: &Path) -> OwnedFd {
    let raw = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
    assert!(raw >= 0, "local cancellation probe requires inotify");
    let watch = unsafe { OwnedFd::from_raw_fd(raw) };
    let path = CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
    assert!(unsafe { libc::inotify_add_watch(raw, path.as_ptr(), libc::IN_ACCESS) } >= 0);
    watch
}

fn scenario(fixture: &RealFixture, case: Case) {
    fixture.verify_closure();
    let root = fixture.0.join(case.name());
    for name in ["source", "output", "work"] { fs::create_dir_all(root.join(name)).unwrap(); }
    let source = match case {
        Case::CompilerError => "export const answer: number = ;\n".to_owned(),
        Case::DeniedProc => "/// <reference path=\"/proc/self/status\" />\nexport const answer = 42;\n".to_owned(),
        Case::DeniedHostInput => {
            // Real readable host input is absent from the compiler's chroot.
            let host = fixture.0.join("host-only.d.ts");
            fs::write(&host, b"declare const hostOnly: number;\n").unwrap();
            format!("/// <reference path=\"{}\" />\nexport const answer = hostOnly;\n", host.display())
        }
        Case::CancelRunning | Case::Deadline => {
            // A real compiler input, never a fake compiler sleep loop. Enough
            // work to observe cancellation/deadline after the observer starts.
            let mut source = MAIN.to_owned();
            for index in 0..150_000 { source.push_str(&format!("export interface Item{index} {{ value: number }}\n")); }
            source
        }
        _ => MAIN.to_owned(),
    };
    fs::write(root.join("source/main.ts"), &source).unwrap();
    let source_fd = directory(&root.join("source")).unwrap();
    let output_fd = directory(&root.join("output")).unwrap();
    let floor = directory(&fixture.0.join("floor")).unwrap();
    let workspace = directory(&root.join("work")).unwrap();
    // Replace the pathnames after capture. The helper must bind the held
    // original inodes, and its writes must survive in that retained output.
    fs::rename(root.join("source"), root.join("source-retained")).unwrap();
    fs::rename(root.join("output"), root.join("output-retained")).unwrap();
    fs::create_dir(root.join("source")).unwrap();
    fs::create_dir(root.join("output")).unwrap();
    fs::write(root.join("source/main.ts"), b"replacement must never compile").unwrap();
    // No supervisor or helper code reads this source after installing the
    // watch. IN_ACCESS therefore observes the actual selected compiler read.
    let access = source_access_watch(&root.join("source-retained/main.ts"));
    let cancellation = eventfd();
    if case == Case::CancelBeforeStart { cancel(&cancellation); }

    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "local real helper fork failed");
    if pid == 0 {
        // Only the disposable worker changes dumpability, NNP and namespaces.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_selected(&root, case, &floor, &workspace, &source_fd, &output_fd, &cancellation)
        }));
        let code = match result {
            Ok(Ok(())) => 0,
            Ok(Err(error)) => { eprintln!("local real SDK {}: {error:#}", case.name()); 1 }
            Err(_) => 2,
        };
        unsafe { libc::_exit(code) }
    }
    let cutoff = Instant::now() + Duration::from_secs(30);
    let mut status = 0;
    let mut cancellation_observed_running = false;
    loop {
        if case == Case::CancelRunning && !cancellation_observed_running {
            let mut events = [0u8; 256];
            let count = unsafe { libc::read(access.as_raw_fd(), events.as_mut_ptr().cast(), events.len()) };
            if count > 0 {
                cancel(&cancellation);
                cancellation_observed_running = true;
            } else {
                assert!(count == -1 && matches!(io::Error::last_os_error().kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted));
            }
        }
        let observed = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if observed == pid { break; }
        assert!(observed == 0 || io::Error::last_os_error().kind() == io::ErrorKind::Interrupted);
        if Instant::now() >= cutoff {
            unsafe { libc::kill(pid, libc::SIGKILL); libc::waitpid(pid, &mut status, 0); }
            panic!("local real helper {} exceeded the test watchdog; no settlement claim", case.name());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0, "real helper {} failed: {status}", case.name());
    if case == Case::CancelRunning { assert!(cancellation_observed_running); }
    assert_eq!(fs::read_to_string(root.join("source-retained/main.ts")).unwrap(), source);
    if case == Case::Compile {
        assert_eq!(fs::read(root.join("output-retained/answer/main.js")).unwrap(), JAVASCRIPT);
    } else if case == Case::CancelBeforeStart {
        assert!(fs::read_dir(root.join("output-retained")).unwrap().next().is_none());
    }
    assert!(fs::read_dir(root.join("output")).unwrap().next().is_none());
    assert_eq!(fs::read(root.join("source/main.ts")).unwrap(), b"replacement must never compile");
    let report: serde_json::Value = serde_json::from_slice(&fs::read(root.join("report.json")).unwrap()).unwrap();
    println!("LOCAL_SDK_REAL_CASE={report}");
}

fn run_selected(root: &Path, case: Case, floor: &OwnedFd, workspace: &OwnedFd,
    source: &OwnedFd, output: &OwnedFd, cancellation: &OwnedFd) -> Result<()> {
    ensure!(unsafe { libc::getuid() == 1500 && libc::geteuid() == 1500 && libc::getgid() == 1500 && libc::getegid() == 1500 }, "actual helper UID/GID differs");
    wire::require_initial_id_maps(std::process::id())?;
    ensure!(unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } == 0, "ND0 unavailable");
    ensure!(unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } == 0, "NNP unavailable");
    let userns = WorkloadUserNamespace::fork()?;
    ensure!(unsafe { libc::fchdir(workspace.as_raw_fd()) } == 0, "retained workspace chdir failed");
    let original = FinalizationBudget::begin(if case == Case::Deadline { Duration::from_millis(500) } else { Duration::from_secs(15) })?;
    let constraint = original.constraint();
    validate_sdk_process_clock(&constraint, false)?;
    let original = FinalizationBudget::from_constraint(&constraint)?;
    ensure!(original.constraint() == constraint, "original deadline changed at adoption");
    let request = serde_json::json!({
        "action_key": {"kind":"blob", "algorithm":"sha256", "digest":digest(b"local-test-only-action"), "size_bytes":b"local-test-only-action".len()},
        "source_root": {"kind":"directory", "algorithm":"sha256", "digest":digest(b"local-test-only-source"), "size_bytes":b"local-test-only-source".len()},
        "profile":{"targets":[{"name":"answer", "language":"typescript", "entrypoint":"main.ts", "sources":["main.ts"]}]}
    });
    let mut stderr = None;
    let result = run_namespace_retaining(userns, floor, workspace, &original,
        Some((&request, source, output, cancellation)), &mut stderr);
    // waitpid is an observation of the kernel's actual child table. It covers
    // the namespace holder AND the real PID1 workload child, including errors.
    let mut status = 0;
    ensure!(unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) } == -1
        && io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD), "actual helper child was not joined");
    ensure!(unsafe { libc::prctl(libc::PR_GET_DUMPABLE, 0, 0, 0, 0) } == 0
        && unsafe { libc::prctl(libc::PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) } == 1, "helper privilege floor changed");
    ensure!(original.constraint() == constraint, "original deadline changed during execution");
    if case == Case::Compile {
        let value = result?;
        ensure!(value["kind"] == "actual-selected-sdk-build-v1" && value["sdk_root_readonly"] == true, "real selected observer did not execute");
        ensure!(value["sdk_python"]["sha256"] == serde_json::from_str::<serde_json::Value>(include_str!("pins.json"))?["python_sha256"], "stand-in interpreter executed");
        ensure!(value["checks"][0]["command"][0] == "/tools/node"
            && value["checks"][0]["exit_code"] == 0 && value["checks"][0]["input_confinement"].is_object(), "real compiler/confinement result absent");
        for value in value["compiler_capabilities"].as_object().context("compiler capability report absent")?.values() {
            ensure!(u64::from_str_radix(value.as_str().context("capability encoding")?, 16)? == 0, "workload retained capabilities");
        }
    } else {
        let error = result.err().context("denied/cancelled/deadline case succeeded")?;
        if matches!(case, Case::CompilerError | Case::DeniedProc | Case::DeniedHostInput) {
            ensure!(error.downcast_ref::<SdkHelperOperation>().is_some_and(|operation| matches!(operation, SdkHelperOperation::WorkloadExit)), "actual workload error was not propagated: {error:#}");
            let error = stderr.as_ref().context("reaped workload stderr absent")?;
            let mut bytes = vec![0; error.metadata()?.len().min(65536) as usize];
            error.read_at(&mut bytes, 0)?;
            let text = String::from_utf8_lossy(&bytes);
            ensure!(text.contains("SDK_ACTUAL_COMPILER_FAILURE"), "actual compiler failure missing: {text}");
            if case == Case::DeniedProc {
                ensure!(text.contains("TS6231") && text.contains("/proc/self/status"), "real TypeScript proc-input refusal absent: {text}");
            }
            if case == Case::DeniedHostInput {
                ensure!(text.contains("TS6053") && text.contains("/workspace/rudder-sdk-real-"), "real TypeScript host-input refusal absent: {text}");
            }
        }
        if matches!(case, Case::CancelBeforeStart | Case::CancelRunning) {
            ensure!(check_cancel(cancellation).is_err(), "original cancellation not preserved");
        }
        if matches!(case, Case::CancelRunning | Case::Deadline) {
            ensure!(stderr.is_some() && root.join("output-retained/answer").is_dir(), "test never reached the actual selected workload");
        }
        if case == Case::Deadline { ensure!(original.remaining().is_err(), "original deadline was refreshed"); }
    }
    let report = serde_json::json!({"kind":"local-real-sdk-execution-v1", "case":case.name(),
        "selected_python":case != Case::CancelBeforeStart, "actual_uid1500_helper":true, "children_joined":true,
        "original_deadline_preserved":true, "nd0":true, "no_new_privileges":true,
        "parent":"test-only-uid1500-stand-in", "sdk_admission":false, "buck2_executed":false,
        "root_custody_qualified":false});
    fs::write(root.join("report.json"), canonical_json(&report)?)?;
    Ok(())
}

#[test]
#[ignore = "explicit local real SDK execution; run scripts/test-sdk-helper-local.sh with pinned tools and enabled user namespaces"]
fn local_real_sdk_selected_execution_and_child_settlement() {
    let fixture = RealFixture::new();
    for case in [Case::Compile, Case::CompilerError, Case::DeniedProc, Case::DeniedHostInput,
        Case::CancelBeforeStart, Case::CancelRunning, Case::Deadline] {
        scenario(&fixture, case);
    }
    fixture.verify_closure();
    println!("RUDDER_LOCAL_REAL_SDK_E2E_V1=passed; SDK admission=false; Root custody=false; Buck2=false");
}
