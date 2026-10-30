//! Fresh-process rootless Linux OverlayFS runner. Never call from App/engine.
//! PID-namespace init is the lifetime boundary: its exit kills and waits out the
//! namespace's remaining processes in the kernel. Only then do we syncfs and
//! capture. SIGSTOP/timestamps are not used as a checkpoint consistency claim.
use super::*;
use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::{
    fs::FileTypeExt,
    net::{UnixListener, UnixStream},
    process::CommandExt,
};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);
extern "C" fn request_stop(_: libc::c_int) {
    STOP_REQUESTED.store(true, Ordering::SeqCst);
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinuxQualification {
    pub supported: bool,
    pub private_user_namespace: bool,
    pub private_mount_namespace: bool,
    pub private_pid_namespace: bool,
    pub overlay_mounted: bool,
    pub terminal_checkpoint: bool,
    pub live_checkpoint: bool,
    pub lower_directory_rename: bool,
    pub hardlink_identity: bool,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelperOutcome {
    pub exit_code: i32,
    pub checkpoint: WorkspaceReceipt,
    pub qualification: LinuxQualification,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalCheckpointRequest {
    pub view_id: String,
    pub owner: OwnerEpoch,
    pub operation_id: String,
    pub seal: bool,
    pub knowledge_cursor: u64,
}

/// Self-contained fresh-executable adapter (also used by isolated qualification
/// drivers). Serialization stays inside the engine crate, avoiding linkage to
/// a caller's independently built serde version.
pub fn run_helper_report(bytes: &[u8]) -> (i32, Vec<u8>) {
    let result = (|| -> Result<HelperOutcome> {
        ensure!(
            bytes.len() <= 64 * 1024,
            "helper specification exceeds 64 KiB"
        );
        run_helper(serde_json::from_slice(bytes)?)
    })();
    match result {
        Ok(outcome) => (outcome.exit_code, serde_json::to_vec(&outcome).unwrap()),
        Err(error) => (
            125,
            serde_json::to_vec(&serde_json::json!({"unsupported":error.to_string()})).unwrap(),
        ),
    }
}

pub fn control_path(manager: &WorkspaceManager, id: &str) -> Result<PathBuf> {
    valid_uuid(id)?;
    let bytes = canonical_json(&(manager.root(), id))?;
    let key = ObjectRef::from_bytes(ObjectKind::Blob, &bytes).digest;
    Ok(PathBuf::from(format!(
        "/tmp/rudder-workspace-{}.sock",
        &key[..24]
    )))
}

pub fn request_terminal_checkpoint(
    manager: &WorkspaceManager,
    request: TerminalCheckpointRequest,
) -> Result<WorkspaceReceipt> {
    valid_uuid(&request.operation_id)?;
    let descriptor = manager.descriptor(&request.view_id, &request.owner)?;
    if let Some(receipt) = descriptor.operations.get(&request.operation_id) {
        ensure!(
            receipt.operation == (if request.seal { "seal" } else { "checkpoint" })
                && receipt.knowledge_cursor == Some(request.knowledge_cursor),
            "operation idempotency conflict"
        );
        return Ok(receipt.clone());
    }
    let mut socket = UnixStream::connect(control_path(manager, &request.view_id)?)?;
    socket.set_read_timeout(Some(Duration::from_secs(300)))?;
    socket.set_write_timeout(Some(Duration::from_secs(5)))?;
    socket.write_all(&canonical_json(&request)?)?;
    socket.shutdown(std::net::Shutdown::Write)?;
    let mut bytes = Vec::new();
    socket.take(64 * 1024 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 64 * 1024, "oversized helper receipt");
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    ensure!(
        value.get("error").is_none(),
        "terminal helper rejected checkpoint request"
    );
    Ok(serde_json::from_value(value)?)
}

/// The caller MUST launch this in a dedicated freshly exec'd single-threaded
/// process. It intentionally rejects a process with any sibling threads before
/// changing namespaces. Every mount target belongs to this manager/view.
pub fn run_helper(spec: WorkspaceHelperSpec) -> Result<HelperOutcome> {
    ensure!(
        fs::read_dir("/proc/self/task")?.count() == 1,
        "namespace setup requires a fresh single-threaded helper process"
    );
    ensure!(
        spec.manager_root.join("manager.json").is_file()
            && spec.store_root.join("retention.json").is_file(),
        "helper requires existing managed workspace/store roots"
    );
    let manager = WorkspaceManager::open(&spec.manager_root, LocalStore::open(&spec.store_root)?)?;
    let run_lock = manager.inactive(&spec.view_id)?;
    let (prepared, descriptor, helper_id) = {
        let _lock = manager.lock()?;
        let mut descriptor = manager.read_descriptor(&spec.view_id)?;
        manager.owner(&descriptor, &spec.owner)?;
        ensure!(!matches!(descriptor.status, WorkspaceStatus::Preparing | WorkspaceStatus::Destroyed | WorkspaceStatus::Sealed | WorkspaceStatus::Running), "workspace is not runnable; interrupted running helpers require explicit completion proof");
        let prepared = manager.prepared(&descriptor.prepared_id)?;
        ensure!(spec.allow_unversioned || spec.vcs_remint_command.is_some() || !prepared.vcs_remint_required, "private Git/jj metadata must be reminted by the trusted VCS bridge before agent execution");
        if prepared.backend != WorkspaceBackend::LinuxOverlay {
            ensure!(
                matches!(
                    descriptor.status,
                    WorkspaceStatus::Materialized | WorkspaceStatus::Quiesced
                ),
                "eager/native workspace must be materialized before running helper"
            );
        }
        let helper_id = Uuid::new_v4().to_string();
        descriptor.status = WorkspaceStatus::Running;
        descriptor.active_helper = Some(helper_id.clone());
        descriptor.revision += 1;
        manager.save(&descriptor)?;
        (prepared, descriptor, helper_id)
    };
    let _state_guard = HelperStateGuard {
        manager: manager.clone(),
        view_id: spec.view_id.clone(),
        owner: spec.owner.clone(),
        helper_id: helper_id.clone(),
    };
    let private = manager.view_path(&spec.view_id)?.join("private");
    ensure_real_directory(&private)?;
    for name in ["upper", "work", "merged"] {
        ensure_real_directory(&private.join(name))?;
    }
    if prepared.backend == WorkspaceBackend::LinuxOverlay {
        ensure_ext4(&private)?;
        for path in prepared
            .lower_layers
            .iter()
            .chain(std::iter::once(&private))
        {
            ensure!(
                !path
                    .as_os_str()
                    .as_bytes()
                    .iter()
                    .any(|byte| b",:\\\0".contains(byte)),
                "overlay paths contain unsupported mount-option separators"
            );
        }
    }
    let socket_path = control_path(&manager, &spec.view_id)?;
    if let Ok(metadata) = fs::symlink_metadata(&socket_path) {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            metadata.file_type().is_socket() && metadata.uid() == unsafe { libc::geteuid() },
            "helper socket path is not owned by this user"
        );
        ensure!(
            UnixStream::connect(&socket_path).is_err(),
            "another helper control socket is live"
        );
        fs::remove_file(&socket_path)?;
    }
    let listener = UnixListener::bind(&socket_path)?;
    fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    let _socket_cleanup = SocketCleanup(socket_path);
    let mut qualification = LinuxQualification {
        supported: false,
        private_user_namespace: false,
        private_mount_namespace: false,
        private_pid_namespace: false,
        overlay_mounted: false,
        terminal_checkpoint: false,
        live_checkpoint: false,
        lower_directory_rename: false,
        hardlink_identity: false,
        reason: None,
    };
    enter_namespaces()?;
    qualification.private_user_namespace = true;
    qualification.private_mount_namespace = true;
    let parent_fd = unsafe { libc::syscall(libc::SYS_pidfd_open, libc::getpid(), 0) };
    ensure!(parent_fd >= 0, "pidfd lifetime fencing is unavailable");
    let parent_lifetime = unsafe { OwnedFd::from_raw_fd(parent_fd as i32) };
    let child = unsafe { libc::fork() };
    ensure!(
        child >= 0,
        "fork PID-namespace init: {}",
        std::io::Error::last_os_error()
    );
    if child == 0 {
        // A dedicated helper's only fork occurs before it starts any threads.
        // The run flock remains inherited by init until namespace teardown.
        let _hold_lock = &run_lock;
        let result = namespace_init(
            &spec,
            &prepared,
            &private,
            &helper_id,
            parent_lifetime.as_raw_fd(),
        );
        let code = match result {
            Ok(code) => code,
            Err(error) => {
                let _ = write_checked(&private.join("helper-error.json"), &error.to_string());
                125
            }
        };
        unsafe { libc::_exit(code.clamp(0, 255)) }
    }
    let mut child_guard = InitGuard(child);
    // The fresh wrapper never owns interactive Ctrl-C/job-control signals once
    // the agent becomes foreground. Only this helper changes its dispositions.
    unsafe {
        libc::signal(libc::SIGTTOU, libc::SIG_IGN);
    }
    qualification.private_pid_namespace = true;
    let mut pending: Option<(TerminalCheckpointRequest, Vec<UnixStream>)> = None;
    let mut status = 0;
    loop {
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
                    let result = (|| -> Result<TerminalCheckpointRequest> {
                        let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
                        let mut size = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
                        ensure!(
                            unsafe {
                                libc::getsockopt(
                                    stream.as_raw_fd(),
                                    libc::SOL_SOCKET,
                                    libc::SO_PEERCRED,
                                    &mut credentials as *mut _ as *mut _,
                                    &mut size,
                                )
                            } == 0
                                && credentials.uid == unsafe { libc::geteuid() },
                            "helper peer ownership mismatch"
                        );
                        let mut bytes = Vec::new();
                        (&mut stream).take(64 * 1024 + 1).read_to_end(&mut bytes)?;
                        ensure!(
                            bytes.len() <= 64 * 1024,
                            "helper control request exceeds limit"
                        );
                        let request: TerminalCheckpointRequest = serde_json::from_slice(&bytes)?;
                        ensure!(
                            request.view_id == spec.view_id && request.owner == descriptor.owner,
                            "helper owner epoch mismatch"
                        );
                        valid_uuid(&request.operation_id)?;
                        let _lock = manager.lock()?;
                        manager.owner(&manager.read_descriptor(&spec.view_id)?, &request.owner)?;
                        Ok(request)
                    })();
                    match result {
                        Ok(request)
                            if pending.as_ref().is_none_or(|(existing, _)| {
                                existing.operation_id == request.operation_id
                                    && existing.seal == request.seal
                                    && existing.knowledge_cursor == request.knowledge_cursor
                            }) =>
                        {
                            if let Some((_, streams)) = pending.as_mut() {
                                if streams.len() < 16 {
                                    streams.push(stream);
                                }
                            } else {
                                pending = Some((request, vec![stream]));
                            }
                            unsafe {
                                libc::kill(child, libc::SIGUSR1);
                            }
                        }
                        _ => {
                            let _ =
                                stream.write_all(b"{\"error\":\"checkpoint request rejected\"}");
                        }
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(error.into()),
            }
        }
        let waited = unsafe { libc::waitpid(child, &mut status, libc::WNOHANG) };
        if waited == child {
            child_guard.0 = 0;
            break;
        }
        if waited < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            bail!("waiting for namespace init failed");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    // Linux zap_pid_ns_processes disables new allocations, kills every thread
    // and waits for the namespace's allocated PID count before init can exit.
    // Reaping that init is stronger than a process-tree polling heuristic.
    if unsafe { libc::isatty(libc::STDIN_FILENO) } == 1 {
        unsafe {
            libc::tcsetpgrp(libc::STDIN_FILENO, libc::getpgrp());
        }
    }
    let visible = private.join("merged");
    let file = File::open(&visible)?;
    ensure!(
        unsafe { libc::syncfs(file.as_raw_fd()) } == 0,
        "syncfs after writer exit failed: {}",
        std::io::Error::last_os_error()
    );
    let failure_path = private.join("helper-error.json");
    if failure_path.try_exists()? {
        let error: String = read_checked(&failure_path)?;
        let _ = fs::remove_file(&failure_path);
        bail!("namespace helper failed: {error}");
    }
    qualification.supported = true;
    qualification.terminal_checkpoint = true;
    qualification.overlay_mounted = prepared.backend == WorkspaceBackend::LinuxOverlay;
    let request = pending
        .as_ref()
        .map(|(request, _)| request.clone())
        .unwrap_or(TerminalCheckpointRequest {
            view_id: spec.view_id.clone(),
            owner: spec.owner.clone(),
            operation_id: helper_id,
            seal: false,
            knowledge_cursor: 0,
        });
    let receipt = {
        let _lock = manager.lock()?;
        let current = manager.read_descriptor(&spec.view_id)?;
        manager.owner(&current, &spec.owner)?;
        ensure!(
            current.active_helper == descriptor.active_helper,
            "helper generation was fenced"
        );
        manager.capture(
            &spec.view_id,
            &spec.owner,
            &request.operation_id,
            request.seal,
            request.knowledge_cursor,
            &visible,
        )?
    };
    if let Some((_, streams)) = pending {
        for mut stream in streams {
            let _ = stream.write_all(&canonical_json(&receipt)?);
        }
    }
    let code = if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else {
        128 + libc::WTERMSIG(status)
    };
    Ok(HelperOutcome {
        exit_code: code,
        checkpoint: receipt,
        qualification,
    })
}

fn namespace_init(
    spec: &WorkspaceHelperSpec,
    prepared: &PreparedWorkspace,
    private: &Path,
    _helper_id: &str,
    parent_lifetime: i32,
) -> Result<i32> {
    ensure!(
        unsafe { libc::getpid() } == 1,
        "helper is not PID-namespace init"
    );
    ensure!(
        unsafe { libc::setpgid(0, 0) } == 0,
        "cannot create namespace-init process group"
    );
    ensure!(
        unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) } == 0,
        "cannot bind helper lifetime to its owner"
    );
    // PID1's parent is outside its namespace, so getppid() legitimately returns
    // zero. A pidfd, not a translated PID/timestamp, closes the PDEATHSIG race.
    let mut parent = libc::pollfd {
        fd: parent_lifetime,
        events: libc::POLLIN,
        revents: 0,
    };
    ensure!(
        unsafe { libc::poll(&mut parent, 1, 0) } == 0,
        "outer helper died before lifetime fencing"
    );
    unsafe {
        libc::signal(
            libc::SIGUSR1,
            request_stop as *const () as libc::sighandler_t,
        );
        libc::signal(libc::SIGTTOU, libc::SIG_IGN);
    }
    let proc_name = CString::new("proc")?;
    let proc_target = CString::new("/proc")?;
    ensure!(
        unsafe {
            libc::mount(
                proc_name.as_ptr(),
                proc_target.as_ptr(),
                proc_name.as_ptr(),
                libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
                std::ptr::null(),
            )
        } == 0,
        "cannot mount private PID procfs: {}",
        std::io::Error::last_os_error()
    );
    let merged = private.join("merged");
    if prepared.backend == WorkspaceBackend::LinuxOverlay {
        mount_overlay(
            &prepared.lower_layers,
            &private.join("upper"),
            &private.join("work"),
            &merged,
        )?;
    }
    if let Some(command) = &spec.vcs_remint_command {
        let result = spawn_command(command, &merged, &spec.view_id)?.wait()?;
        ensure!(result.success(), "trusted VCS remint bridge failed");
    }
    let mut command = spawn_command(&spec.command, &merged, &spec.view_id)?;
    let mut stopping = None;
    let code = loop {
        if let Some(status) = command.try_wait()? {
            use std::os::unix::process::ExitStatusExt;
            break status.code().unwrap_or(128 + status.signal().unwrap_or(9));
        }
        if STOP_REQUESTED.load(Ordering::SeqCst) {
            let began = stopping.get_or_insert_with(|| {
                unsafe {
                    libc::kill(-1, libc::SIGTERM);
                }
                Instant::now()
            });
            if began.elapsed() > Duration::from_secs(2) {
                unsafe {
                    libc::kill(-1, libc::SIGKILL);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    unsafe {
        libc::kill(-1, libc::SIGKILL);
    }
    loop {
        let mut status = 0;
        let result = unsafe { libc::waitpid(-1, &mut status, 0) };
        if result < 0 {
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD) {
                break;
            }
            if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                bail!("namespace descendant reap failed");
            }
        }
    }
    Ok(code)
}

fn spawn_command(arguments: &[String], cwd: &Path, view_id: &str) -> Result<std::process::Child> {
    ensure!(!arguments.is_empty(), "helper command is empty");
    let mut command = Command::new(&arguments[0]);
    command
        .args(&arguments[1..])
        .current_dir(cwd)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .env("RUDDER_WORKSPACE_VIEW_ID", view_id);
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            libc::signal(libc::SIGTTOU, libc::SIG_DFL);
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            #[repr(C)]
            struct Header {
                version: u32,
                pid: i32,
            }
            #[repr(C)]
            struct Data {
                effective: u32,
                permitted: u32,
                inheritable: u32,
            }
            let header = Header {
                version: 0x20080522,
                pid: 0,
            };
            let data = [
                Data {
                    effective: 0,
                    permitted: 0,
                    inheritable: 0,
                },
                Data {
                    effective: 0,
                    permitted: 0,
                    inheritable: 0,
                },
            ];
            if libc::syscall(libc::SYS_capset, &header, data.as_ptr()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn()?;
    if unsafe { libc::isatty(libc::STDIN_FILENO) } == 1 {
        ensure!(
            unsafe { libc::tcsetpgrp(libc::STDIN_FILENO, child.id() as i32) } == 0,
            "cannot give agent foreground PTY ownership"
        );
        unsafe {
            libc::kill(-(child.id() as i32), libc::SIGCONT);
        }
    }
    Ok(child)
}

fn enter_namespaces() -> Result<()> {
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    ensure!(
        unsafe { libc::unshare(libc::CLONE_NEWUSER) } == 0,
        "user namespaces unavailable: {}",
        std::io::Error::last_os_error()
    );
    match fs::write("/proc/self/setgroups", "deny") {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    fs::write("/proc/self/uid_map", format!("{uid} {uid} 1\n"))?;
    fs::write("/proc/self/gid_map", format!("{gid} {gid} 1\n"))?;
    ensure!(
        unsafe { libc::unshare(libc::CLONE_NEWNS | libc::CLONE_NEWPID) } == 0,
        "mount/PID namespaces unavailable: {}",
        std::io::Error::last_os_error()
    );
    let root = CString::new("/")?;
    ensure!(
        unsafe {
            libc::mount(
                std::ptr::null(),
                root.as_ptr(),
                std::ptr::null(),
                libc::MS_REC | libc::MS_PRIVATE,
                std::ptr::null(),
            )
        } == 0,
        "cannot make helper mounts private: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}

fn mount_overlay(lowers: &[PathBuf], upper: &Path, work: &Path, merged: &Path) -> Result<()> {
    for lower in lowers {
        let lower_c = CString::new(lower.as_os_str().as_bytes())?;
        ensure!(
            unsafe {
                libc::mount(
                    lower_c.as_ptr(),
                    lower_c.as_ptr(),
                    std::ptr::null(),
                    libc::MS_BIND | libc::MS_REC,
                    std::ptr::null(),
                )
            } == 0,
            "bind immutable lower failed: {}",
            std::io::Error::last_os_error()
        );
        ensure!(
            unsafe {
                libc::mount(
                    std::ptr::null(),
                    lower_c.as_ptr(),
                    std::ptr::null(),
                    libc::MS_BIND
                        | libc::MS_REMOUNT
                        | libc::MS_RDONLY
                        | libc::MS_NOSUID
                        | libc::MS_NODEV,
                    std::ptr::null(),
                )
            } == 0,
            "read-only lower bind failed: {}",
            std::io::Error::last_os_error()
        );
    }
    // Kernel userxattr mode forbids redirect_dir=on. Inherited directory rename
    // returns EXDEV; report that capability gap instead of claiming POSIX parity.
    let mut data = b"lowerdir=".to_vec();
    for (index, lower) in lowers.iter().enumerate() {
        if index > 0 {
            data.push(b':');
        }
        data.extend_from_slice(lower.as_os_str().as_bytes());
    }
    data.extend_from_slice(b",upperdir=");
    data.extend_from_slice(upper.as_os_str().as_bytes());
    data.extend_from_slice(b",workdir=");
    data.extend_from_slice(work.as_os_str().as_bytes());
    data.extend_from_slice(b",userxattr,index=off,metacopy=off,redirect_dir=nofollow");
    let data = CString::new(data)?;
    let target = CString::new(merged.as_os_str().as_bytes())?;
    let overlay = CString::new("overlay")?;
    ensure!(
        unsafe {
            libc::mount(
                overlay.as_ptr(),
                target.as_ptr(),
                overlay.as_ptr(),
                libc::MS_NOSUID | libc::MS_NODEV,
                data.as_ptr() as *const _,
            )
        } == 0,
        "rootless OverlayFS mount unavailable: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}

fn ensure_ext4(path: &Path) -> Result<()> {
    let path = CString::new(path.as_os_str().as_bytes())?;
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    ensure!(unsafe { libc::statfs(path.as_ptr(), &mut stat) } == 0 && stat.f_type as u64 == 0xef53, "Linux overlay backend is only qualified on ext4; select explicit eager materialization otherwise");
    Ok(())
}
struct SocketCleanup(PathBuf);
impl Drop for SocketCleanup {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
struct InitGuard(libc::pid_t);
impl Drop for InitGuard {
    fn drop(&mut self) {
        if self.0 > 0 {
            unsafe {
                libc::kill(self.0, libc::SIGKILL);
                let mut status = 0;
                libc::waitpid(self.0, &mut status, 0);
            }
        }
    }
}
struct HelperStateGuard {
    manager: WorkspaceManager,
    view_id: String,
    owner: OwnerEpoch,
    helper_id: String,
}
impl Drop for HelperStateGuard {
    fn drop(&mut self) {
        let _ = (|| -> Result<()> {
            let _lock = self.manager.lock()?;
            let mut descriptor = self.manager.read_descriptor(&self.view_id)?;
            if descriptor.owner == self.owner
                && descriptor.active_helper.as_deref() == Some(self.helper_id.as_str())
                && descriptor.status == WorkspaceStatus::Running
            {
                descriptor.status = WorkspaceStatus::Orphaned;
                descriptor.active_helper = None;
                descriptor.revision += 1;
                self.manager.save(&descriptor)?;
            }
            Ok(())
        })();
    }
}
