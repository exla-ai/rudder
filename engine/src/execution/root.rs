//! Restricted-root construction in the fresh PID namespace's init. No agent
//! code runs until every mount is hardened and the old root is detached.
use super::*;
use anyhow::{bail, Context};
use std::{
    collections::BTreeSet,
    ffi::CString,
    fs::{self, File, OpenOptions},
    os::{
        fd::AsRawFd,
        unix::{
            ffi::OsStrExt,
            fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
        },
    },
    path::{Component, Path},
};

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Layout {
    pub root: PathBuf,
    pub home: PathBuf,
    pub tmp: PathBuf,
    pub scratch: PathBuf,
    pub view: PathBuf,
}
fn c(path: &Path) -> Result<CString> {
    Ok(CString::new(path.as_os_str().as_bytes())?)
}
fn syscall(result: i32, phase: &str) -> Result<()> {
    if result != 0 {
        return Err(std::io::Error::last_os_error()).context(phase.to_owned());
    }
    Ok(())
}
pub(crate) fn guest_relative(value: &str) -> Result<PathBuf> {
    let p = Path::new(value);
    ensure!(
        p.is_absolute() && p != Path::new("/") && !value.contains('\0'),
        "guest projection must be a non-root absolute path"
    );
    ensure!(
        p.components()
            .all(|p| matches!(p, Component::RootDir | Component::Normal(_))),
        "unsafe guest projection path"
    );
    Ok(p.strip_prefix("/")?.to_path_buf())
}
fn public_runtime(source: &Path) -> bool {
    [
        "/usr",
        "/opt/runtime",
        "/opt/rudder",
        "/opt/rudder-worker",
        "/etc/ssl/certs",
        "/etc/alternatives",
    ]
    .iter()
    .any(|p| source.starts_with(p))
        || [
            "/etc/ld.so.cache",
            "/etc/nsswitch.conf",
            "/etc/resolv.conf",
            "/etc/hosts",
            "/etc/localtime",
        ]
        .iter()
        .any(|p| source == Path::new(p))
}
pub(crate) fn validate(spec: &ExecutionSpec) -> Result<()> {
    fn normalized(path: &Path) -> Result<PathBuf> {
        ensure!(
            path.is_absolute()
                && path
                    .components()
                    .all(|c| matches!(c, Component::RootDir | Component::Normal(_))),
            "unsafe authority path"
        );
        let mut parent = path.to_path_buf();
        let mut suffix = Vec::new();
        while !parent.try_exists()? {
            suffix.push(
                parent
                    .file_name()
                    .context("authority path lacks existing ancestor")?
                    .to_os_string(),
            );
            ensure!(parent.pop(), "authority path lacks root");
        }
        let mut canonical = parent.canonicalize()?;
        for component in suffix.into_iter().rev() {
            canonical.push(component);
        }
        Ok(canonical)
    }
    let denied = [&spec.manager_root, &spec.store_root, &spec.execution_root]
        .into_iter()
        .chain(spec.authority_roots.iter())
        .map(|p| normalized(p))
        .collect::<Result<Vec<_>>>()?;
    let mut guests = BTreeSet::<PathBuf>::new();
    for r in &spec.runtime {
        let guest = guest_relative(&r.guest_path)?;
        ensure!(
            !["/etc/passwd", "/etc/group"].contains(&r.guest_path.as_str()),
            "guest identity files are generated privately"
        );
        ensure!(
            r.guest_path == "/usr"
                || r.guest_path.starts_with("/usr/")
                || r.guest_path == "/opt/runtime"
                || r.guest_path.starts_with("/opt/runtime/")
                || r.guest_path.starts_with("/etc/"),
            "runtime guest target is not an approved public runtime location"
        );
        ensure!(
            guests
                .iter()
                .all(|p| !p.starts_with(&guest) && !guest.starts_with(p)),
            "overlapping runtime projections"
        );
        guests.insert(guest);
        let source = r
            .source
            .canonicalize()
            .context("runtime source unavailable")?;
        ensure!(
            public_runtime(&source),
            "runtime source is outside approved public system/image prefixes"
        );
        ensure!(
            denied
                .iter()
                .all(|p| !source.starts_with(p) && !p.starts_with(&source)),
            "runtime projection overlaps private authority state"
        );
        if let Some(broker) = &spec.broker_directory {
            let broker = broker.canonicalize()?;
            ensure!(
                !source.starts_with(&broker) && !broker.starts_with(&source),
                "runtime projection overlaps agent identity state"
            );
        }
        let m = fs::metadata(source)?;
        ensure!(
            m.is_dir() || m.is_file(),
            "runtime projection is not a file/directory"
        );
    }
    let mut homes = BTreeSet::new();
    for file in &spec.home_files {
        let path = Path::new(&file.relative_path);
        ensure!(
            !path.is_absolute()
                && !file.relative_path.is_empty()
                && path.components().all(|c| matches!(c, Component::Normal(_)))
                && !file.relative_path.contains('\0'),
            "unsafe private home seed path"
        );
        ensure!(
            homes
                .iter()
                .all(|p: &PathBuf| !p.starts_with(path) && !path.starts_with(p)),
            "overlapping private home seeds"
        );
        homes.insert(path.to_path_buf());
    }
    if let Some(path) = &spec.broker_directory {
        validate_broker(path)?;
        validate_broker_binding(
            path,
            spec.broker_binding
                .as_ref()
                .context("broker binding absent")?,
        )?;
    }
    Ok(())
}
pub(crate) fn validate_broker(path: &Path) -> Result<()> {
    let m = fs::symlink_metadata(path)?;
    ensure!(
        m.is_dir()
            && !m.file_type().is_symlink()
            && m.uid() == unsafe { libc::geteuid() }
            && m.mode() & 0o777 == 0o700,
        "broker directory must be owned private directory"
    );
    let mut identity = false;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let m = fs::symlink_metadata(entry.path())?;
        ensure!(
            m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o777 == 0o600,
            "broker projection has unsafe ownership/mode"
        );
        if entry.file_name() == "identity.json" {
            ensure!(
                m.is_file()
                    && !m.file_type().is_symlink()
                    && m.nlink() == 1
                    && m.len() <= 64 * 1024,
                "broker identity is not a bounded private regular file"
            );
            identity = true;
        } else if entry.file_name() == "broker.sock" {
            ensure!(
                m.file_type().is_socket(),
                "broker endpoint must be a socket"
            );
        } else {
            bail!("broker directory contains an unexpected entry")
        }
    }
    ensure!(
        identity,
        "broker identity is required; journal fallback is prohibited"
    );
    Ok(())
}
fn validate_broker_binding(path: &Path, expected: &BrokerBinding) -> Result<()> {
    use std::io::Read;
    let mut bytes = Vec::new();
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path.join("identity.json"))?
        .take(16 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 16 * 1024, "broker identity exceeds16KiB");
    let actual: crate::agent_api::AgentIdentityFile = serde_json::from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("broker identity has an invalid schema"))?;
    ensure!(
        actual.format_version == 1
            && actual.project_id == expected.project_id
            && actual.run_id == expected.run_id
            && actual.run_epoch == expected.run_epoch
            && actual.owner_epoch == expected.owner_epoch
            && !actual.token.is_empty()
            && actual.token.len() <= 4096,
        "broker identity binding differs from execution attempt"
    );
    Ok(())
}
pub(crate) fn private_dir(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(m) => ensure!(
            m.is_dir() && !m.file_type().is_symlink() && m.uid() == unsafe { libc::geteuid() },
            "private execution path has wrong type/owner"
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(path)?;
            File::open(path.parent().context("private directory parent")?)?.sync_all()?;
        }
        Err(e) => return Err(e.into()),
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}
fn target(root: &Path, guest: &str, dir: bool) -> Result<PathBuf> {
    let relative = guest_relative(guest)?;
    let mut current = root.to_path_buf();
    let count = relative.components().count();
    for (i, part) in relative.components().enumerate() {
        let Component::Normal(part) = part else {
            bail!("invalid guest component")
        };
        current.push(part);
        let directory = i + 1 < count || dir;
        match fs::symlink_metadata(&current) {
            Ok(m) => ensure!(
                !m.file_type().is_symlink() && (if directory { m.is_dir() } else { m.is_file() }),
                "guest projection traverses wrong type/symlink"
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if directory {
                    fs::create_dir(&current)?;
                } else {
                    OpenOptions::new()
                        .create_new(true)
                        .write(true)
                        .mode(0o600)
                        .open(&current)?;
                }
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(current)
}
fn harden(path: &Path, readonly: bool, devices: bool) -> Result<()> {
    #[repr(C)]
    struct Attr {
        set: u64,
        clear: u64,
        propagation: u64,
        userns: u64,
    }
    let fd = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)?;
    let attr = Attr {
        set: 2 | if devices { 0 } else { 4 } | if readonly { 1 } else { 0 },
        clear: 0,
        propagation: 0,
        userns: 0,
    };
    let result = unsafe {
        libc::syscall(
            libc::SYS_mount_setattr,
            fd.as_raw_fd(),
            c"".as_ptr(),
            0x1000 | 0x8000,
            &attr,
            std::mem::size_of::<Attr>(),
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error())
            .context("recursive runtime mount hardening unavailable");
    };
    Ok(())
}
fn bind(source: &Path, destination: &Path, readonly: bool, devices: bool) -> Result<()> {
    // Pin source inode before mount, including runtime files. This prevents a
    // path rename/symlink swap from redirecting the trusted projection.
    let fd = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(source)?;
    let source = PathBuf::from(format!("/proc/self/fd/{}", fd.as_raw_fd()));
    syscall(
        unsafe {
            libc::mount(
                c(&source)?.as_ptr(),
                c(destination)?.as_ptr(),
                std::ptr::null(),
                libc::MS_BIND | libc::MS_REC,
                std::ptr::null(),
            )
        },
        "bind selected runtime/private projection",
    )?;
    harden(destination, readonly, devices)
}
pub(crate) fn seed_home(layout: &Layout, spec: &ExecutionSpec) -> Result<()> {
    use std::io::Write;
    for file in &spec.home_files {
        let path = layout.home.join(&file.relative_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut f = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)?;
        f.write_all(&file.contents)?;
        f.sync_all()?;
        File::open(path.parent().unwrap())?.sync_all()?;
    }
    Ok(())
}
pub(crate) fn restrict(layout: &Layout, spec: &ExecutionSpec) -> Result<()> {
    ensure!(
        unsafe { libc::getpid() } == 1,
        "restricted root must be constructed by private PID init"
    );
    let root = c(&layout.root)?;
    syscall(
        unsafe {
            libc::mount(
                c"tmpfs".as_ptr(),
                root.as_ptr(),
                c"tmpfs".as_ptr(),
                libc::MS_NOSUID | libc::MS_NODEV,
                c"size=32m,nr_inodes=4096,mode=700".as_ptr().cast(),
            )
        },
        "mount private root tmpfs",
    )?;
    // Create all destinations before any read-only parent binds appear.
    for r in &spec.runtime {
        target(
            &layout.root,
            &r.guest_path,
            fs::metadata(&r.source)?.is_dir(),
        )?;
    }
    for guest in [
        "/workspace",
        "/home/agent",
        "/tmp",
        "/scratch",
        "/proc",
        "/dev",
        "/dev/pts",
        "/run/rudder/agent",
        "/.old-root",
        "/etc",
    ] {
        target(&layout.root, guest, true)?;
    }
    for device in ["null", "zero", "random", "urandom", "tty"] {
        target(&layout.root, &format!("/dev/{device}"), false)?;
    }
    for r in &spec.runtime {
        let source = r.source.canonicalize()?;
        bind(
            &source,
            &layout.root.join(guest_relative(&r.guest_path)?),
            true,
            false,
        )?;
    }
    bind(&layout.view, &layout.root.join("workspace"), false, false)?;
    bind(&layout.home, &layout.root.join("home/agent"), false, false)?;
    bind(&layout.tmp, &layout.root.join("tmp"), false, false)?;
    bind(&layout.scratch, &layout.root.join("scratch"), false, false)?;
    if let Some(broker) = &spec.broker_directory {
        validate_broker(broker)?;
        validate_broker_binding(
            broker,
            spec.broker_binding
                .as_ref()
                .context("broker binding absent")?,
        )?;
        bind(broker, &layout.root.join("run/rudder/agent"), true, false)?;
    }
    for device in ["null", "zero", "random", "urandom", "tty"] {
        let source = PathBuf::from(format!("/dev/{device}"));
        ensure!(
            fs::symlink_metadata(&source)?.file_type().is_char_device(),
            "runtime device is not a character device"
        );
        bind(
            &source,
            &layout.root.join(format!("dev/{device}")),
            false,
            true,
        )?;
    }
    let devpts = layout.root.join("dev/pts");
    let options = CString::new(format!(
        "newinstance,gid={},mode=620,ptmxmode=666",
        unsafe { libc::getgid() }
    ))?;
    syscall(
        unsafe {
            libc::mount(
                c"devpts".as_ptr(),
                c(&devpts)?.as_ptr(),
                c"devpts".as_ptr(),
                libc::MS_NOSUID | libc::MS_NOEXEC,
                options.as_ptr().cast(),
            )
        },
        "mount private devpts",
    )?;
    for (name, target) in [
        ("bin", "usr/bin"),
        ("lib", "usr/lib"),
        ("lib64", "usr/lib64"),
        ("dev/ptmx", "pts/ptmx"),
        ("dev/fd", "/proc/self/fd"),
        ("dev/stdin", "/proc/self/fd/0"),
        ("dev/stdout", "/proc/self/fd/1"),
        ("dev/stderr", "/proc/self/fd/2"),
    ] {
        let p = layout.root.join(name);
        ensure!(
            !p.try_exists()?,
            "runtime aliases overlap an existing projection"
        );
        std::os::unix::fs::symlink(target, p)?;
    }
    // Guest identity is synthetic and scoped, rather than host passwd/group.
    fs::write(
        layout.root.join("etc/passwd"),
        format!(
            "agent:x:{}:{}:agent:/home/agent:/bin/sh\n",
            unsafe { libc::getuid() },
            unsafe { libc::getgid() }
        ),
    )?;
    fs::write(
        layout.root.join("etc/group"),
        format!("agent:x:{}:\n", unsafe { libc::getgid() }),
    )?;
    let proc = layout.root.join("proc");
    syscall(
        unsafe {
            libc::mount(
                c"proc".as_ptr(),
                c(&proc)?.as_ptr(),
                c"proc".as_ptr(),
                libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC | libc::MS_RDONLY,
                std::ptr::null(),
            )
        },
        "mount namespace-only read-only procfs",
    )?;
    // Make just the root tmpfs read-only: writable child bind mounts remain so.
    syscall(
        unsafe {
            libc::mount(
                std::ptr::null(),
                root.as_ptr(),
                std::ptr::null(),
                libc::MS_REMOUNT | libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV,
                c"size=32m,nr_inodes=4096,mode=700".as_ptr().cast(),
            )
        },
        "seal restricted root",
    )?;
    std::env::set_current_dir(&layout.root)?;
    syscall(
        unsafe { libc::syscall(libc::SYS_pivot_root, c".".as_ptr(), c".old-root".as_ptr()) as i32 },
        "pivot to restricted root",
    )?;
    std::env::set_current_dir("/")?;
    syscall(
        unsafe { libc::umount2(c"/.old-root".as_ptr(), libc::MNT_DETACH) },
        "detach inaccessible old root",
    )?;
    std::env::set_current_dir("/workspace")?;
    Ok(())
}
