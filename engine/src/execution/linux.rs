//! Fresh-process Linux implementation; never called from a native App thread.
use super::*;
use anyhow::Context;
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
};
pub fn sealed_spec_fd(spec: &ExecutionSpec) -> Result<OwnedFd> {
    spec.validate()?;
    let bytes = crate::protocol::canonical_json(spec)?;
    ensure!(
        bytes.len() <= 1024 * 1024,
        "execution specification exceeds1MiB"
    );
    let raw = unsafe {
        libc::syscall(
            libc::SYS_memfd_create,
            c"rudder-execution-spec".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        )
    };
    ensure!(raw >= 0, "sealed execution memory is unavailable");
    let mut file = unsafe { File::from_raw_fd(raw as i32) };
    file.write_all(&bytes)?;
    file.seek(SeekFrom::Start(0))?;
    let seals = libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;
    ensure!(
        unsafe { libc::fcntl(file.as_raw_fd(), libc::F_ADD_SEALS, seals) } == 0,
        "cannot seal execution specification"
    );
    Ok(file.into())
}
pub fn read_spec_fd(fd: OwnedFd) -> Result<ExecutionSpec> {
    let seals = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GET_SEALS) };
    let required = libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;
    ensure!(
        seals >= 0 && seals & required == required,
        "execution specification FD is not fully sealed"
    );
    let mut file = File::from(fd);
    let meta = file.metadata()?;
    ensure!(
        meta.is_file() && meta.len() <= 1024 * 1024,
        "invalid execution specification FD size/type"
    );
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= 1024 * 1024,
        "execution specification exceeds1MiB"
    );
    let spec: ExecutionSpec =
        serde_json::from_slice(&bytes).context("invalid sealed execution specification")?;
    spec.validate()?;
    Ok(spec)
}
pub fn run_helper(_spec: ExecutionSpec) -> Result<ExecutionOutcome> {
    supervisor(_spec)
}

use super::{root::Layout, state::State};
use crate::{
    store::LocalStore,
    vfs::{
        linux::{mount, PrivateMountNamespace},
        VfsManager,
    },
};
use std::{
    fs,
    os::{
        fd::RawFd,
        unix::{
            net::UnixStream,
            process::{CommandExt, ExitStatusExt},
        },
    },
    process::{Command, Stdio},
    time::{Duration, Instant},
};

#[derive(Serialize, Deserialize)]
struct ChildSpec {
    spec: ExecutionSpec,
    layout: Layout,
    nonce: String,
}
#[derive(Serialize, Deserialize)]
struct InitReport {
    nonce: String,
    agent_exit: i32,
    started: bool,
    error: Option<String>,
}
#[derive(Serialize, Deserialize)]
struct DrainReport {
    nonce: String,
    agent_exit: i32,
    kernel_drained: bool,
}

pub fn arm_parent(parent: u32) -> Result<OwnedFd> {
    ensure!(
        parent > 1 && unsafe { libc::getppid() } == parent as i32,
        "execution parent is no longer its owner"
    );
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, parent, 0) };
    ensure!(fd >= 0, "execution parent lifetime descriptor unavailable");
    let fd = unsafe { OwnedFd::from_raw_fd(fd as i32) };
    ensure!(
        unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) } == 0,
        "cannot fence execution parent death"
    );
    check_lifetime(fd.as_raw_fd())?;
    Ok(fd)
}
fn check_lifetime(fd: RawFd) -> Result<()> {
    let mut p = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    ensure!(
        unsafe { libc::poll(&mut p, 1, 0) } == 0,
        "execution parent died during initialization"
    );
    Ok(())
}
fn proof() -> ExecutionIsolation {
    ExecutionIsolation {
        same_uid_user_namespace: true,
        private_mount_namespace: true,
        kernel_pid_namespace_drained: true,
        restricted_root: true,
        read_only_runtime: true,
        inherited_authority_fds_closed: true,
        no_new_privileges: true,
        network_isolated: false,
        cgroup_limited: false,
    }
}
fn supervisor(spec: ExecutionSpec) -> Result<ExecutionOutcome> {
    spec.validate()?;
    ensure!(
        unsafe { libc::geteuid() } == 1500 && unsafe { libc::getegid() } == 1500,
        "execution profile requires worker UID/GID1500"
    );
    ensure!(
        fs::read_dir("/proc/self/task")?.count() == 1,
        "execution supervisor must be a fresh helper"
    );
    let parent = unsafe { libc::getppid() } as u32;
    let lifetime = arm_parent(parent)?;
    let namespace = PrivateMountNamespace::enter_for_current_user()?;
    check_lifetime(lifetime.as_raw_fd())?;
    ensure!(
        unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0) } == 0,
        "cannot protect supervisor descriptors"
    );
    unsafe {
        libc::signal(libc::SIGTTOU, libc::SIG_IGN);
    }
    let manager = VfsManager::open(&spec.manager_root, LocalStore::open(&spec.store_root)?)?;
    let mut state = State::open(&spec)?;
    if state.record.phase == ExecutionPhase::Checkpointed {
        ensure!(
            state.record.checkpoint.is_some(),
            "checkpointed execution lacks retained root"
        );
        return Ok(ExecutionOutcome {
            record: state.record,
            isolation: proof(),
            warnings: Vec::new(),
        });
    }
    ensure!(
        state.is_new,
        "execution has uncertain prior effects; reconcile its orphan before retry"
    );
    let result = (|| -> Result<ExecutionOutcome> {
        let view = manager.open_view(&spec.view_id, &spec.owner)?;
        let mountpoint = manager.root().join("mounts").join(&spec.view_id);
        let layout = Layout {
            root: state.directory.join("root"),
            home: state.directory.join("home"),
            tmp: state.directory.join("tmp"),
            scratch: state.directory.join("scratch"),
            view: mountpoint.clone(),
        };
        for path in [&layout.root, &layout.home, &layout.tmp, &layout.scratch] {
            super::root::private_dir(path)?;
        }
        super::root::seed_home(&layout, &spec)?;
        let lease = manager.store().begin_staging(Duration::from_secs(3600))?;
        let result = (|| -> Result<ExecutionOutcome> {
            let mut mounted = mount(view, &mountpoint, &namespace)?;
            let nonce = Uuid::new_v4().to_string();
            let mut projected = spec.clone();
            projected.home_files.clear();
            let childspec = ChildSpec {
                spec: projected,
                layout,
                nonce: nonce.clone(),
            };
            let payload = sealed_payload(&childspec)?;
            let (mut receive, send) = UnixStream::pair()?;
            receive.set_read_timeout(Some(Duration::from_secs(3)))?;
            let fd3 = duplicate_high(payload.as_raw_fd())?;
            let fd4 = duplicate_high(send.as_raw_fd())?;
            let mut command = Command::new("/proc/self/exe");
            command
                .args(["fabric", "execution", "child"])
                .stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit());
            unsafe {
                command.pre_exec(move || {
                    if libc::dup2(fd3.as_raw_fd(), 3) < 0
                        || libc::dup2(fd4.as_raw_fd(), 4) < 0
                        || libc::setpgid(0, 0) != 0
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                    if libc::syscall(libc::SYS_close_range, 5u32, u32::MAX, 4u32) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            state.record.phase = ExecutionPhase::Running;
            state.save()?;
            let mut child = command
                .spawn()
                .context("spawn restricted execution init wrapper")?;
            drop(command);
            drop(send);
            drop(payload);
            let mut guard = ProcessGuard(child.id() as i32);
            let began = Instant::now();
            let mut renewed = Instant::now();
            let status = loop {
                if let Some(status) = child.try_wait()? {
                    guard.0 = 0;
                    break status;
                }
                if began.elapsed() > Duration::from_secs(spec.timeout_seconds + 30) {
                    let _ = child.kill();
                    let _ = child.wait();
                    guard.0 = 0;
                    anyhow::bail!("restricted execution drain deadline exceeded")
                }
                if renewed.elapsed() > Duration::from_secs(60) {
                    manager
                        .store()
                        .renew_staging(&lease.id, Duration::from_secs(3600))?;
                    renewed = Instant::now();
                }
                std::thread::sleep(Duration::from_millis(10));
            };
            ensure!(
                status.success(),
                "execution init wrapper failed; no checkpoint can be acknowledged"
            );
            let mut bytes = Vec::new();
            (&mut receive).take(64 * 1024 + 1).read_to_end(&mut bytes)?;
            ensure!(
                bytes.len() <= 64 * 1024,
                "execution drain report exceeds limit"
            );
            let drain: DrainReport = serde_json::from_slice(&bytes)
                .context("missing execution namespace-drain proof")?;
            ensure!(
                drain.nonce == nonce && drain.kernel_drained,
                "execution namespace-drain proof mismatch"
            );
            if unsafe { libc::isatty(0) } == 1 {
                unsafe {
                    libc::tcsetpgrp(0, libc::getpgrp());
                }
            }
            let view = mounted.unmount()?;
            let checkpoint = view.checkpoint_snapshot_unmounted()?;
            state.record.phase = ExecutionPhase::Checkpointed;
            state.record.agent_exit = Some(drain.agent_exit);
            state.record.checkpoint = Some(checkpoint);
            state.save()?;
            Ok(ExecutionOutcome {
                record: state.record.clone(),
                isolation: proof(),
                warnings: Vec::new(),
            })
        })();
        let release = manager.store().abandon_staging(&lease.id);
        match result {
            Ok(mut value) => {
                if release.is_err() {
                    value.warnings.push(
                        "checkpoint committed; execution staging cleanup needs reconciliation"
                            .into(),
                    );
                }
                Ok(value)
            }
            Err(e) => Err(e),
        }
    })();
    if result.is_err() && state.record.phase != ExecutionPhase::Checkpointed {
        state.record.phase = ExecutionPhase::Orphaned;
        let _ = state.save();
    }
    result
}

fn duplicate_high(fd: RawFd) -> Result<OwnedFd> {
    let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 10) };
    if duplicate < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(duplicate) })
}
fn sealed_payload<T: Serialize>(value: &T) -> Result<OwnedFd> {
    let bytes = crate::protocol::canonical_json(value)?;
    ensure!(
        bytes.len() <= 1024 * 1024,
        "execution child payload exceeds1MiB"
    );
    let raw = unsafe {
        libc::syscall(
            libc::SYS_memfd_create,
            c"rudder-execution-child".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        )
    };
    ensure!(raw >= 0, "execution child memfd unavailable");
    let mut file = unsafe { File::from_raw_fd(raw as i32) };
    file.write_all(&bytes)?;
    file.seek(SeekFrom::Start(0))?;
    ensure!(
        unsafe {
            libc::fcntl(
                file.as_raw_fd(),
                libc::F_ADD_SEALS,
                libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL,
            )
        } == 0,
        "cannot seal child payload"
    );
    Ok(file.into())
}
fn read_payload(fd: OwnedFd) -> Result<ChildSpec> {
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GET_SEALS) };
    let required = libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;
    ensure!(
        flags >= 0 && flags & required == required,
        "child payload FD is not sealed"
    );
    let mut file = File::from(fd);
    ensure!(
        file.metadata()?.len() <= 1024 * 1024,
        "child payload too large"
    );
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 1024 * 1024, "child payload exceeds1MiB");
    Ok(serde_json::from_slice(&bytes)?)
}

/// Internal fresh-exec entry; only sealed FD3 and private report FD4 survive.
pub fn run_child() -> Result<()> {
    ensure!(
        fs::read_dir("/proc/self/task")?.count() == 1,
        "execution child wrapper must be fresh"
    );
    let payload = read_payload(unsafe { OwnedFd::from_raw_fd(3) })?;
    payload.spec.validate()?;
    let mut report = unsafe { File::from_raw_fd(4) };
    let parent = unsafe { libc::getppid() } as u32;
    let lifetime = arm_parent(parent)?;
    let _namespace = PrivateMountNamespace::enter_for_current_user()?;
    check_lifetime(lifetime.as_raw_fd())?;
    ensure!(
        unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0) } == 0,
        "cannot protect wrapper descriptors"
    );
    ensure!(
        unsafe { libc::unshare(libc::CLONE_NEWPID) } == 0,
        "private PID namespace unavailable"
    );
    let wrapper = unsafe { libc::syscall(libc::SYS_pidfd_open, libc::getpid(), 0) };
    ensure!(wrapper >= 0, "PID init lifetime descriptor unavailable");
    let wrapper = unsafe { OwnedFd::from_raw_fd(wrapper as i32) };
    let (mut init_receive, init_send) = UnixStream::pair()?;
    init_receive.set_read_timeout(Some(Duration::from_secs(2)))?;
    let init = unsafe { libc::fork() };
    ensure!(init >= 0, "cannot fork namespace init");
    if init == 0 {
        drop(report);
        drop(init_receive);
        let mut pipe = init_send;
        let result = namespace_init(&payload, wrapper.as_raw_fd());
        let code = match result {
            Ok(exit) => {
                let frame = InitReport {
                    nonce: payload.nonce.clone(),
                    agent_exit: exit,
                    started: true,
                    error: None,
                };
                if pipe
                    .write_all(&crate::protocol::canonical_json(&frame).unwrap_or_default())
                    .is_ok()
                {
                    0
                } else {
                    125
                }
            }
            Err(error) => {
                let frame = InitReport {
                    nonce: payload.nonce.clone(),
                    agent_exit: 125,
                    started: false,
                    error: Some(format!("{error:#}")),
                };
                let _ =
                    pipe.write_all(&crate::protocol::canonical_json(&frame).unwrap_or_default());
                125
            }
        };
        unsafe { libc::_exit(code) }
    }
    drop(init_send);
    let mut guard = ProcessGuard(init);
    let mut status = 0;
    loop {
        let result = unsafe { libc::waitpid(init, &mut status, 0) };
        if result == init {
            guard.0 = 0;
            break;
        }
        if result < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            anyhow::bail!("cannot reap PID namespace init")
        }
    }
    let mut bytes = Vec::new();
    (&mut init_receive)
        .take(64 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 64 * 1024, "init outcome too large");
    let outcome: InitReport = serde_json::from_slice(&bytes)?;
    if !libc::WIFEXITED(status) || libc::WEXITSTATUS(status) != 0 {
        anyhow::bail!(
            "namespace init failed: {}",
            outcome
                .error
                .as_deref()
                .unwrap_or("unexpected process termination")
        )
    }
    ensure!(
        outcome.started && outcome.nonce == payload.nonce,
        "init outcome mismatch"
    );
    // Reaping PID1 happens only after zap_pid_ns_processes kills all remaining
    // namespace threads and waits for kernel teardown, including async I/O.
    let frame = DrainReport {
        nonce: payload.nonce,
        agent_exit: outcome.agent_exit,
        kernel_drained: true,
    };
    report.write_all(&crate::protocol::canonical_json(&frame)?)?;
    Ok(())
}
fn namespace_init(payload: &ChildSpec, parent: RawFd) -> Result<i32> {
    ensure!(
        unsafe { libc::getpid() } == 1,
        "execution is not private PID namespace init"
    );
    ensure!(
        unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) } == 0,
        "cannot fence PID init lifetime"
    );
    check_lifetime(parent)?;
    ensure!(
        unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0) } == 0,
        "cannot protect init descriptors"
    );
    ensure!(
        unsafe { libc::setpgid(0, 0) } == 0,
        "cannot isolate init process group"
    );
    unsafe {
        libc::signal(libc::SIGTTOU, libc::SIG_IGN);
    }
    super::root::restrict(&payload.layout, &payload.spec)?;
    let mut command = Command::new(&payload.spec.command[0]);
    command
        .args(&payload.spec.command[1..])
        .env_clear()
        .envs(&payload.spec.environment)
        .env("HOME", "/home/agent")
        .env("TMPDIR", "/tmp")
        .env("PWD", "/workspace")
        .env("RUDDER_EXECUTION_SANDBOX", "1")
        .env("RUDDER_WORKSPACE_VIEW_ID", &payload.spec.view_id)
        .current_dir("/workspace")
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    if !payload.spec.environment.contains_key("PATH") {
        command.env("PATH", "/usr/bin:/bin");
    }
    if !payload.spec.environment.contains_key("TERM") {
        command.env("TERM", "xterm-256color");
    }
    if payload.spec.broker_directory.is_some() {
        command
            .env(
                "RUDDER_AGENT_BROKER_SOCKET",
                "/run/rudder/agent/broker.sock",
            )
            .env("RUDDER_AGENT_IDENTITY", "/run/rudder/agent/identity.json");
    }
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            libc::signal(libc::SIGTTOU, libc::SIG_DFL);
            if libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 4u32) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            drop_privileges()?;
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .context("launch command in restricted root")?;
    if unsafe { libc::isatty(0) } == 1 {
        ensure!(
            unsafe { libc::tcsetpgrp(0, child.id() as i32) } == 0,
            "cannot give agent foreground PTY"
        );
        unsafe {
            libc::kill(-(child.id() as i32), libc::SIGCONT);
        }
    }
    let began = Instant::now();
    let exit = loop {
        if let Some(status) = child.try_wait()? {
            break status.code().unwrap_or(128 + status.signal().unwrap_or(9));
        }
        if began.elapsed() > Duration::from_secs(payload.spec.timeout_seconds) {
            break 124;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    // PID1's exit is the final kernel-enforced kill/reap barrier. Userland
    // attempts termination too; detached/nested namespace escape cannot evade
    // zap_pid_ns_processes when this init exits.
    unsafe {
        libc::kill(-1, libc::SIGKILL);
    }
    Ok(exit)
}
fn drop_privileges() -> std::io::Result<()> {
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
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0
        || unsafe { libc::syscall(libc::SYS_capset, &header, data.as_ptr()) } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}
struct ProcessGuard(i32);
impl Drop for ProcessGuard {
    fn drop(&mut self) {
        if self.0 > 0 {
            unsafe {
                libc::kill(self.0, libc::SIGKILL);
                let mut status = 0;
                while libc::waitpid(self.0, &mut status, 0) < 0
                    && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
                {
                }
            }
        }
    }
}
