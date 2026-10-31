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
    let workspace_endpoint = spec
        .workspace_observation
        .as_ref()
        .map(|workspace| normalized(&workspace.endpoint_directory))
        .transpose()?;
    let application = spec
        .workspace_observation
        .as_ref()
        .and_then(|w| w.application.as_ref());
    let application_endpoint = application
        .map(|a| normalized(&a.projection_directory))
        .transpose()?;
    if let (Some(application), Some(endpoint)) = (application, application_endpoint.as_ref()) {
        let context = normalized(&application.host_context.path)?;
        ensure!(
            spec.authority_roots
                .iter()
                .map(|p| normalized(p))
                .collect::<Result<Vec<_>>>()?
                .iter()
                .any(|root| context.starts_with(root)),
            "protected application context must be an explicit authority root"
        );
        let parent = fs::symlink_metadata(
            endpoint
                .parent()
                .context("application endpoint parent absent")?,
        )?;
        ensure!(
            parent.is_dir()
                && parent.uid() == unsafe { libc::geteuid() }
                && parent.mode() & 0o777 == 0o700,
            "application endpoint parent must be private and owned"
        );
        let mut protected = denied.clone();
        protected.push(context);
        if let Some(workspace) = &workspace_endpoint {
            protected.push(workspace.clone());
        }
        if let Some(broker) = &spec.broker_directory {
            protected.push(normalized(broker)?);
        }
        ensure!(
            protected
                .iter()
                .all(|p| !endpoint.starts_with(p) && !p.starts_with(endpoint)),
            "application endpoint overlaps private state or another service"
        );
    }
    if let Some(endpoint) = &workspace_endpoint {
        let parent = endpoint
            .parent()
            .context("workspace endpoint parent absent")?;
        let metadata = fs::symlink_metadata(parent)?;
        ensure!(
            metadata.is_dir()
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o777 == 0o700,
            "workspace endpoint parent must be private and owned"
        );
        for authority in [&spec.manager_root, &spec.store_root]
            .into_iter()
            .chain(spec.authority_roots.iter())
        {
            let authority = normalized(authority)?;
            ensure!(
                !endpoint.starts_with(&authority) && !authority.starts_with(endpoint),
                "workspace endpoint overlaps private authority state"
            );
        }
        let execution = normalized(&spec.execution_root)?;
        ensure!(
            !execution.starts_with(endpoint),
            "workspace endpoint cannot contain execution records"
        );
        if let Some(broker) = &spec.broker_directory {
            let broker = normalized(broker)?;
            ensure!(
                !endpoint.starts_with(&broker) && !broker.starts_with(endpoint),
                "workspace endpoint and agent broker projections must be separate"
            );
        }
    }
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
        if let Some(endpoint) = &workspace_endpoint {
            ensure!(
                !source.starts_with(endpoint) && !endpoint.starts_with(&source),
                "runtime projection overlaps workspace identity state"
            );
        }
        if let Some(endpoint) = &application_endpoint {
            ensure!(
                !source.starts_with(endpoint) && !endpoint.starts_with(&source),
                "runtime projection overlaps application service"
            );
        }
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

pub(crate) fn validate_workspace_projection(workspace: &WorkspaceObservationProfile) -> Result<()> {
    let path = &workspace.endpoint_directory;
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir()
            && !metadata.file_type().is_symlink()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o777 == 0o700,
        "workspace projection must be an owned private directory"
    );
    let mut identity_found = false;
    let mut socket_found = false;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o777 == 0o600,
            "workspace projection has unsafe ownership/mode"
        );
        if entry.file_name() == "identity.json" {
            let identity = crate::vfs::rpc::read_identity(&entry.path())?;
            ensure!(
                identity.binding == workspace.binding,
                "workspace projection identity differs from execution"
            );
            identity_found = true;
        } else if entry.file_name() == "workspace.sock" {
            ensure!(
                metadata.file_type().is_socket(),
                "workspace endpoint must be a socket"
            );
            socket_found = true;
        } else {
            bail!("workspace projection contains an unexpected entry");
        }
    }
    ensure!(
        identity_found && socket_found,
        "owning workspace RPC must be ready before guest launch"
    );
    Ok(())
}

pub(crate) fn validate_partial_workspace_projection(
    workspace: &WorkspaceObservationProfile,
    expected: &crate::vfs::partial_rpc::PartialBinding,
) -> Result<()> {
    use crate::vfs::partial_rpc::{self, IDENTITY_NAME, SOCKET_NAME};
    expected.validate()?;
    ensure!(
        expected.observation == workspace.binding && workspace.application.is_none(),
        "partial workspace projection differs from the sealed source-only assignment"
    );
    let path = &workspace.endpoint_directory;
    let directory = fs::symlink_metadata(path)?;
    ensure!(
        directory.is_dir()
            && !directory.file_type().is_symlink()
            && directory.uid() == unsafe { libc::geteuid() }
            && directory.mode() & 0o777 == 0o700,
        "partial workspace projection must be an owned private directory"
    );
    let mut identity_found = false;
    let mut socket_found = false;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        ensure!(
            !metadata.file_type().is_symlink()
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o777 == 0o600
                && metadata.nlink() == 1,
            "partial workspace projection has unsafe ownership/mode/links"
        );
        if entry.file_name() == IDENTITY_NAME {
            ensure!(
                metadata.is_file(),
                "partial workspace identity must be a regular file"
            );
            let identity = partial_rpc::read_identity(&entry.path())?;
            ensure!(
                identity.binding == *expected,
                "partial workspace identity differs from the exact execution binding"
            );
            identity_found = true;
        } else if entry.file_name() == SOCKET_NAME {
            ensure!(
                metadata.file_type().is_socket(),
                "partial workspace endpoint must be a socket"
            );
            socket_found = true;
        } else {
            bail!("partial workspace projection contains an unexpected entry");
        }
    }
    ensure!(
        identity_found && socket_found,
        "owning partial workspace RPC must be ready before guest launch"
    );
    Ok(())
}

#[cfg(test)]
mod partial_projection_tests {
    use super::*;
    use crate::{
        protocol::{canonical_json, ObjectKind, ObjectRef, OwnerEpoch},
        remote_cache::CacheScope,
        remote_root::{AttemptBinding, RootBinding},
        vfs::{
            partial_rpc::{self, PartialBinding, PartialIdentity},
            rpc::{AttemptContext, ObservationBinding},
        },
    };
    use std::os::unix::{fs::symlink, net::UnixListener};
    struct Fixture {
        _temp: tempfile::TempDir,
        _listener: UnixListener,
        workspace: WorkspaceObservationProfile,
        binding: PartialBinding,
    }
    impl Fixture {
        fn new() -> Self {
            let base = if Path::new("/workspace").is_dir() {
                Path::new("/workspace")
            } else {
                Path::new("/tmp")
            };
            let temp = tempfile::Builder::new()
                .prefix("rpp-")
                .tempdir_in(base)
                .unwrap();
            let directory = temp.path().join("w");
            fs::create_dir(&directory).unwrap();
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
            let owner = OwnerEpoch {
                owner: "projection-owner".into(),
                epoch: 1,
            };
            let execution = uuid::Uuid::new_v4().to_string();
            let operation = uuid::Uuid::new_v4().to_string();
            let view = uuid::Uuid::new_v4().to_string();
            let observation = ObservationBinding {
                format_version: 1,
                attempt: AttemptContext {
                    execution_id: execution.clone(),
                    operation_id: operation.clone(),
                },
                view_id: view.clone(),
                owner: owner.clone(),
                namespace: uuid::Uuid::new_v4().to_string(),
                profile: ObjectRef::from_bytes(ObjectKind::Blob, b"profile"),
                exclusion_policy_digest: crate::vfs::exclusion_policy_digest(
                    &crate::store::ImportPolicy::default(),
                )
                .unwrap(),
            };
            let binding = PartialBinding {
                protocol: partial_rpc::PROTOCOL.into(),
                format_version: 1,
                launch_request_digest: "a".repeat(64),
                root: RootBinding {
                    scope: CacheScope {
                        provider_id: "projection".into(),
                        tenant_id: "tenant".into(),
                    },
                    root_id: "root".into(),
                    root: ObjectRef::from_bytes(ObjectKind::RunnableRoot, b"runnable"),
                },
                attempt: AttemptBinding {
                    project_id: "project".into(),
                    run_id: "run".into(),
                    run_epoch: 1,
                    owner,
                    execution_id: execution,
                    operation_id: operation,
                    view_id: view,
                },
                observation: observation.clone(),
            };
            let listener = UnixListener::bind(directory.join(partial_rpc::SOCKET_NAME)).unwrap();
            fs::set_permissions(
                directory.join(partial_rpc::SOCKET_NAME),
                fs::Permissions::from_mode(0o600),
            )
            .unwrap();
            let fixture = Self {
                _temp: temp,
                _listener: listener,
                workspace: WorkspaceObservationProfile {
                    binding: observation,
                    endpoint_directory: directory,
                    application: None,
                },
                binding,
            };
            fixture.write_identity(&fixture.binding);
            fixture
        }
        fn write_identity(&self, binding: &PartialBinding) {
            let identity = PartialIdentity {
                protocol: partial_rpc::PROTOCOL.into(),
                format_version: 1,
                binding: binding.clone(),
                token: "c".repeat(64),
            };
            let path = self
                .workspace
                .endpoint_directory
                .join(partial_rpc::IDENTITY_NAME);
            fs::write(&path, canonical_json(&identity).unwrap()).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        fn validate(&self) -> Result<()> {
            validate_partial_workspace_projection(&self.workspace, &self.binding)
        }
    }
    #[test]
    fn exact_partial_projection_passes_while_legacy_family_stays_separate() {
        let fixture = Fixture::new();
        fixture.validate().unwrap();
        assert!(validate_workspace_projection(&fixture.workspace).is_err());
    }
    #[test]
    fn valid_legacy_identity_and_names_never_satisfy_partial_projection() {
        let fixture = Fixture::new();
        let directory = &fixture.workspace.endpoint_directory;
        let identity = crate::vfs::rpc::ObservationIdentity {
            format_version: 1,
            binding: fixture.workspace.binding.clone(),
            token: "c".repeat(64),
        };
        fs::write(
            directory.join(partial_rpc::IDENTITY_NAME),
            canonical_json(&identity).unwrap(),
        )
        .unwrap();
        assert!(fixture.validate().is_err());
        fs::rename(
            directory.join(partial_rpc::IDENTITY_NAME),
            directory.join("identity.json"),
        )
        .unwrap();
        fs::rename(
            directory.join(partial_rpc::SOCKET_NAME),
            directory.join("workspace.sock"),
        )
        .unwrap();
        validate_workspace_projection(&fixture.workspace).unwrap();
        assert!(fixture.validate().is_err());
    }
    #[test]
    fn extra_entries_unsafe_modes_and_symlinked_leaf_or_directory_are_rejected() {
        let fixture = Fixture::new();
        let directory = &fixture.workspace.endpoint_directory;
        let extra = directory.join("extra");
        fs::write(&extra, b"unapproved").unwrap();
        fs::set_permissions(&extra, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(fixture.validate().is_err());
        fs::remove_file(extra).unwrap();
        let identity = directory.join(partial_rpc::IDENTITY_NAME);
        fs::set_permissions(&identity, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(fixture.validate().is_err());
        fs::set_permissions(&identity, fs::Permissions::from_mode(0o600)).unwrap();
        let original = fixture._temp.path().join("held-identity.json");
        fs::rename(&identity, &original).unwrap();
        symlink(&original, &identity).unwrap();
        assert!(fixture.validate().is_err());
        fs::remove_file(&identity).unwrap();
        fs::rename(&original, &identity).unwrap();
        fixture.validate().unwrap();
        let original_directory = fixture._temp.path().join("held-directory");
        fs::rename(directory, &original_directory).unwrap();
        symlink(&original_directory, directory).unwrap();
        assert!(fixture.validate().is_err());
    }
    #[test]
    fn matching_observation_cannot_hide_stale_root_or_whole_selection_digest() {
        let fixture = Fixture::new();
        for field in ["root", "digest", "namespace"] {
            let mut changed = fixture.binding.clone();
            match field {
                "root" => changed.root.root_id = "stale-root".into(),
                "digest" => changed.launch_request_digest = "b".repeat(64),
                _ => changed.observation.namespace = uuid::Uuid::new_v4().to_string(),
            }
            changed.validate().unwrap();
            fixture.write_identity(&changed);
            assert!(fixture.validate().is_err());
        }
        fixture.write_identity(&fixture.binding);
        fixture.validate().unwrap();
    }
}

pub(crate) fn validate_application_projection(
    spec: &ExecutionSpec,
    projection: &GuestServiceProjection,
) -> Result<()> {
    projection.validate(spec)?;
    let metadata = fs::symlink_metadata(&projection.directory)?;
    ensure!(
        metadata.is_dir()
            && !metadata.file_type().is_symlink()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o777 == 0o700,
        "application projection must be a private owned directory"
    );
    let mut expected: std::collections::BTreeMap<_, _> = projection
        .entries
        .iter()
        .map(|e| (e.name.as_str(), &e.node))
        .collect();
    for entry in fs::read_dir(&projection.directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name
            .to_str()
            .context("application entry name is not UTF-8")?;
        let kind = expected
            .remove(name)
            .context("application projection contains an unexpected entry")?;
        let metadata = fs::symlink_metadata(entry.path())?;
        ensure!(
            !metadata.file_type().is_symlink()
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o777 == 0o600
                && metadata.nlink() == 1,
            "unsafe application projection ownership/mode/link count"
        );
        match kind {
            ServiceProjectionKind::File { max_size_bytes } => ensure!(
                metadata.is_file() && metadata.len() <= *max_size_bytes,
                "application projected file type/size differs"
            ),
            ServiceProjectionKind::Socket => ensure!(
                metadata.file_type().is_socket(),
                "application endpoint is not a socket"
            ),
        }
    }
    ensure!(
        expected.is_empty(),
        "application projection is missing an entry"
    );
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
pub(crate) fn restrict(
    layout: &Layout,
    spec: &ExecutionSpec,
    application: Option<&GuestServiceProjection>,
) -> Result<()> {
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
    if spec.workspace_observation.is_some() {
        target(&layout.root, WORKSPACE_GUEST_DIRECTORY, true)?;
    }
    if let Some(projection) = application {
        validate_application_projection(spec, projection)?;
        target(
            &layout.root,
            &spec
                .workspace_observation
                .as_ref()
                .unwrap()
                .application
                .as_ref()
                .unwrap()
                .guest_directory(),
            true,
        )?;
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
    if let Some(workspace) = &spec.workspace_observation {
        let partial = spec
            .profile
            .as_ref()
            .map(|profile| profile.partial_binding())
            .transpose()?
            .flatten();
        if let Some(expected) = &partial {
            validate_partial_workspace_projection(workspace, expected)?;
        } else {
            validate_workspace_projection(workspace)?;
        }
        bind(
            &workspace.endpoint_directory,
            &layout.root.join("run/rudder/workspace"),
            true,
            false,
        )?;
    }
    if let Some(projection) = application {
        validate_application_projection(spec, projection)?;
        let guest = spec
            .workspace_observation
            .as_ref()
            .unwrap()
            .application
            .as_ref()
            .unwrap()
            .guest_directory();
        bind(
            &projection.directory,
            &layout.root.join(guest_relative(&guest)?),
            true,
            false,
        )?;
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
