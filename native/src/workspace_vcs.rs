//! Explicit v3 workspace VCS composition. No dashboard/default launch switch.
//! Only the fresh execution helper calls this factory after namespace entry.
#[cfg(not(target_os = "linux"))]
use anyhow::bail;
use anyhow::{ensure, Context, Result};
use rudder_engine::execution::{
    ExecutionSpec, GuestServiceProjection, HelperApplication, HelperApplicationSpec,
    HelperFinalizationGuard, HelperServices, ProtectedHostContextRef, ServiceProjectionEntry,
    ServiceProjectionKind, WorkspaceRpcReady,
};
use rudder_engine::protocol::canonical_json;
use rudder_engine::vfs::rpc::{AttemptAuthority, ObservationBinding};
use rudder_vcs_bridge::command_transport::{
    CommandClientConfig, CommandService, CommandServiceConfig,
};
use rudder_vcs_bridge::{host, vfs_provider, AttemptBinding};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

pub(crate) const KIND: &str = "workspace-vcs-v1";
const MAX_CONTEXT: u64 = 256 * 1024;
const MAX_EXECUTABLE: u64 = 256 * 1024 * 1024;

fn hex_bytes(bytes: impl AsRef<[u8]>) -> String {
    bytes
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExecutableIdentity {
    pub sha256: String,
    pub size_bytes: u64,
}
/// Contains paths and immutable bindings, never credentials. It is written
/// outside any guest projection, then SHA-bound by a sealed execution spec.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkspaceVcsContext {
    pub format_version: u32,
    pub kind: String,
    pub service: CommandServiceConfig,
    pub executable: ExecutableIdentity,
    pub guest_executable: String,
}

/// Normal launch preparation resolves only the packaged sibling. Explicit
/// already-pinned context paths remain available to frozen qualification.
pub(crate) fn context_with_companion(
    service: CommandServiceConfig,
    guest_executable: String,
) -> Result<WorkspaceVcsContext> {
    let native = std::env::current_exe()?;
    context_for_executable(&native, service, guest_executable)
}
fn context_for_executable(
    native: &Path,
    mut service: CommandServiceConfig,
    guest_executable: String,
) -> Result<WorkspaceVcsContext> {
    let binary = native
        .parent()
        .context("native companion directory absent")?
        .join("rudder-jj");
    service.binary = binary;
    let mut file = open_protected(&service.binary)
        .context("packaged rudder-jj companion is unavailable; no fallback is permitted")?;
    let (sha256, size_bytes) = digest(&mut file, MAX_EXECUTABLE)?;
    let context = WorkspaceVcsContext {
        format_version: 1,
        kind: KIND.into(),
        service,
        executable: ExecutableIdentity { sha256, size_bytes },
        guest_executable,
    };
    pin_executable(&context)?;
    context.service.validate()?;
    Ok(context)
}

fn normalized(path: &Path) -> Result<()> {
    ensure!(
        path.is_absolute()
            && path != Path::new("/")
            && !path.as_os_str().as_encoded_bytes().contains(&0)
            && path
                .components()
                .all(|c| matches!(c, Component::RootDir | Component::Normal(_))),
        "unsafe workspace VCS path"
    );
    Ok(())
}
/// Descriptor traversal prevents a symlink in any component redirecting a
/// protected context read. Do not chmod or follow an existing caller path.
fn open_protected(path: &Path) -> Result<File> {
    normalized(path)?;
    let mut directory = File::open("/")?;
    let parts: Vec<_> = path
        .components()
        .filter_map(|c| match c {
            Component::Normal(p) => Some(p),
            _ => None,
        })
        .collect();
    for (index, part) in parts.iter().enumerate() {
        use std::os::unix::ffi::OsStrExt;
        let name = std::ffi::CString::new(part.as_bytes())?;
        let mut flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        if index + 1 < parts.len() {
            flags |= libc::O_DIRECTORY;
        }
        let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error())
                .context("open protected workspace VCS path");
        }
        directory = unsafe { File::from_raw_fd(fd) };
    }
    Ok(directory)
}
fn private_directory(path: &Path, create: bool) -> Result<()> {
    normalized(path)?;
    if create && !path.try_exists()? {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)?;
    }
    let file = open_protected(path)?;
    let meta = file.metadata()?;
    ensure!(
        meta.is_dir() && meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o777 == 0o700,
        "workspace VCS directory must be private and owned"
    );
    Ok(())
}
fn digest(file: &mut File, limit: u64) -> Result<(String, u64)> {
    let size = file.metadata()?.len();
    ensure!(
        size <= limit,
        "protected workspace VCS file exceeds size bound"
    );
    file.seek(SeekFrom::Start(0))?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 128 * 1024];
    let mut total = 0u64;
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        ensure!(
            total <= limit,
            "protected workspace VCS file grew beyond bound"
        );
        hash.update(&buffer[..n]);
    }
    ensure!(
        total == size && file.metadata()?.len() == size,
        "protected workspace VCS file changed while reading"
    );
    Ok((hex_bytes(hash.finalize()), size))
}
fn read_context(reference: &ProtectedHostContextRef) -> Result<WorkspaceVcsContext> {
    reference.validate()?;
    let file = open_protected(&reference.path)?;
    let meta = file.metadata()?;
    ensure!(
        meta.is_file()
            && meta.uid() == unsafe { libc::geteuid() }
            && meta.mode() & 0o777 == 0o600
            && meta.nlink() == 1
            && meta.len() == reference.size_bytes,
        "unsafe protected workspace VCS context"
    );
    let mut bytes = Vec::new();
    file.take(MAX_CONTEXT + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 == reference.size_bytes
            && hex_bytes(Sha256::digest(&bytes)) == reference.sha256,
        "workspace VCS context hash/size changed"
    );
    let context: WorkspaceVcsContext =
        serde_json::from_slice(&bytes).context("invalid protected workspace VCS context")?;
    ensure!(
        canonical_json(&context)? == bytes,
        "workspace VCS context must be canonical"
    );
    ensure!(
        context.format_version == 1 && context.kind == KIND,
        "unsupported workspace VCS context"
    );
    Ok(context)
}

/// Caller is trusted background launch preparation. Existing context bytes are
/// immutable: repeated preparation may verify the same bytes, never replace
/// another attempt's configuration. This file is not published into shared CAS.
pub(crate) fn write_host_context(
    path: &Path,
    context: &WorkspaceVcsContext,
) -> Result<ProtectedHostContextRef> {
    ensure!(
        context.format_version == 1 && context.kind == KIND,
        "unsupported workspace VCS context"
    );
    context.service.validate()?;
    private_directory(path.parent().context("host context parent absent")?, true)?;
    let bytes = canonical_json(context)?;
    ensure!(
        !bytes.is_empty() && bytes.len() as u64 <= MAX_CONTEXT,
        "workspace VCS context exceeds bound"
    );
    let reference = ProtectedHostContextRef {
        format_version: 1,
        path: path.into(),
        sha256: hex_bytes(Sha256::digest(&bytes)),
        size_bytes: bytes.len() as u64,
    };
    if path.try_exists()? {
        read_context(&reference)?;
        return Ok(reference);
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    File::open(path.parent().unwrap())?.sync_all()?;
    Ok(reference)
}

fn covered(path: &Path, roots: &[PathBuf]) -> Result<bool> {
    normalized(path)?;
    Ok(roots.iter().any(|root| path.starts_with(root)))
}
fn existing_or_parent(path: &Path) -> Result<PathBuf> {
    normalized(path)?;
    let mut base = path.to_path_buf();
    let mut suffix = Vec::new();
    while !base.try_exists()? {
        suffix.push(
            base.file_name()
                .context("private path ancestor absent")?
                .to_os_string(),
        );
        ensure!(base.pop(), "private path has no ancestor");
    }
    open_protected(&base)?;
    let mut result = base.canonicalize()?;
    for name in suffix.into_iter().rev() {
        result.push(name);
    }
    ensure!(
        result == path,
        "private workspace VCS path is not canonical"
    );
    Ok(result)
}
fn validate_assignment(
    context: &WorkspaceVcsContext,
    spec: &ExecutionSpec,
    application: &HelperApplicationSpec,
) -> Result<()> {
    ensure!(
        application.kind == KIND && context.kind == KIND && context.format_version == 1,
        "unsupported workspace VCS application"
    );
    context.service.validate()?;
    let workspace = spec
        .workspace_observation
        .as_ref()
        .context("workspace observation absent")?;
    let broker = spec
        .broker_binding
        .as_ref()
        .context("sealed broker binding absent")?;
    let view = &context.service.view;
    let attempt = &view.attempt;
    ensure!(
        view.observation == workspace.binding
            && attempt.project_id == broker.project_id
            && attempt.run_id == broker.run_id
            && attempt.run_epoch == broker.run_epoch
            && attempt.owner == spec.owner
            && attempt.owner.epoch == broker.owner_epoch
            && attempt.attempt_id == spec.execution_id,
        "workspace VCS context belongs to another run/owner/attempt"
    );
    ensure!(
        view.socket == workspace.endpoint_directory.join("workspace.sock")
            && view.identity_file == workspace.endpoint_directory.join("identity.json"),
        "workspace VCS context changed the owning RPC endpoint"
    );
    ensure!(
        view.cas_root == spec.store_root && existing_or_parent(&view.cas_root)? == view.cas_root,
        "workspace VCS context changed its private CAS root"
    );
    ensure!(
        context.service.command_timeout_ms <= 20_000,
        "native VCS command timeout exceeds20seconds"
    );
    let private = [
        &view.private_repository,
        &view.projection_state,
        &context.service.state_directory,
    ];
    for path in private {
        existing_or_parent(path)?;
        ensure!(
            covered(path, &spec.authority_roots)?,
            "private VCS path is not a declared authority root"
        );
        ensure!(
            !path.starts_with(&spec.store_root)
                && !spec.store_root.starts_with(path)
                && !path.starts_with(&spec.manager_root)
                && !spec.manager_root.starts_with(path)
                && !path.starts_with(&spec.execution_root)
                && !spec.execution_root.starts_with(path),
            "private VCS path overlaps workspace/CAS/execution authority"
        );
    }
    for (index, a) in private.iter().enumerate() {
        for b in private.iter().skip(index + 1) {
            ensure!(
                !a.starts_with(b) && !b.starts_with(a),
                "mutable private VCS roots overlap"
            );
        }
    }
    ensure!(
        view.private_git.starts_with(&view.private_repository),
        "private Git store escapes private metadata root"
    );
    existing_or_parent(&view.private_git)?;
    private_directory(&view.private_repository, false)?;
    ensure!(
        application.projection_directory != view.private_repository
            && !application
                .projection_directory
                .starts_with(&view.private_repository),
        "guest service exposes private metadata"
    );
    ensure!(
        application
            .projection_directory
            .join("jj.sock")
            .as_os_str()
            .as_encoded_bytes()
            .len()
            < 100,
        "VCS command endpoint exceeds portable socket bound"
    );
    let expected_environment = [
        (
            "RUDDER_JJ_CLIENT_CONFIG",
            format!("{}/client.json", application.guest_directory()),
        ),
        ("RUDDER_JJ_BIN", context.guest_executable.clone()),
    ];
    for (key, value) in expected_environment {
        ensure!(
            spec.environment
                .get(key)
                .is_none_or(|actual| actual == &value),
            "guest VCS environment conflicts with protected application"
        );
    }
    for key in ["RUDDER_JJ_HOST_CONTEXT", "RUDDER_JJ_WRITER_FD"] {
        ensure!(
            !spec.environment.contains_key(key),
            "guest cannot supply host VCS execution context"
        );
    }
    normalized(Path::new(&context.guest_executable))?;
    let mut mapped = false;
    for runtime in &spec.runtime {
        let source = runtime.source.canonicalize()?;
        if source.is_file() && source == context.service.binary {
            mapped |= Path::new(&runtime.guest_path) == Path::new(&context.guest_executable);
        } else if source.is_dir() && context.service.binary.starts_with(&source) {
            mapped |= Path::new(&runtime.guest_path)
                .join(context.service.binary.strip_prefix(&source)?)
                == Path::new(&context.guest_executable);
        }
    }
    ensure!(
        mapped,
        "thin jj executable is not an exact read-only guest runtime projection"
    );
    Ok(())
}
fn pin_executable(context: &WorkspaceVcsContext) -> Result<File> {
    let mut file = open_protected(&context.service.binary)?;
    let meta = file.metadata()?;
    // Root-owned image files appear as overflowuid after the single-UID user
    // namespace transition. The trusted image path, SHA/length and held inode
    // are the executable binding; no guest may choose or write this file.
    let overflow_uid = fs::read_to_string("/proc/sys/kernel/overflowuid")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok());
    let owner_allowed = meta.uid() == 0
        || overflow_uid == Some(meta.uid())
        || (meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o222 == 0);
    ensure!(
        meta.is_file()
            && owner_allowed
            && meta.mode() & 0o022 == 0
            && meta.mode() & 0o111 != 0
            && meta.nlink() == 1,
        "unsafe managed jj executable"
    );
    let (sha256, size) = digest(&mut file, MAX_EXECUTABLE)?;
    ensure!(
        sha256 == context.executable.sha256 && size == context.executable.size_bytes,
        "managed jj executable hash/size changed"
    );
    file.seek(SeekFrom::Start(0))?;
    let mut elf = [0; 4];
    file.read_exact(&mut elf)?;
    ensure!(
        elf == *b"\x7fELF",
        "native Linux managed jj profile requires a pinned ELF executable"
    );
    Ok(file)
}

struct BrokerAuthority {
    broker: crate::agent_broker::BrokerClient,
    observation: ObservationBinding,
    attempt: Option<AttemptBinding>,
}
impl AttemptAuthority for BrokerAuthority {
    fn validate(&self, binding: &ObservationBinding) -> Result<()> {
        ensure!(
            *binding == self.observation,
            "workspace authority belongs to another exact execution"
        );
        self.broker.authorize_workspace(binding)
    }
}
impl vfs_provider::AttemptAuthority for BrokerAuthority {
    fn validate(&self, binding: &AttemptBinding) -> Result<()> {
        ensure!(
            self.attempt.as_ref() == Some(binding),
            "VCS authority belongs to another private attempt"
        );
        AttemptAuthority::validate(self, &self.observation)
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn services(_spec: &ExecutionSpec) -> Result<HelperServices> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "workspace services require a qualified Linux worker",
    )
    .into())
}

#[cfg(target_os = "linux")]
pub(crate) fn services(spec: &ExecutionSpec) -> Result<HelperServices> {
    let workspace = spec
        .workspace_observation
        .as_ref()
        .context("workspace observation absent")?;
    if let Some(application) = &workspace.application {
        ensure!(
            cfg!(target_os = "linux") && application.kind == KIND,
            "unsupported workspace application kind/platform"
        );
    }
    let directory = spec
        .broker_directory
        .as_ref()
        .context("scoped broker projection absent")?;
    let expected = spec
        .broker_binding
        .as_ref()
        .context("sealed broker binding absent")?;
    let expected = crate::agent_broker::BrokerBinding {
        project_id: expected.project_id.clone(),
        run_id: expected.run_id.clone(),
        run_epoch: expected.run_epoch,
        owner_epoch: expected.owner_epoch,
    };
    let broker = crate::agent_broker::BrokerClient::connect_workspace(
        directory.join("broker.sock"),
        directory.join("identity.json"),
        &expected,
        &workspace.binding,
    )?;
    let Some(application) = &workspace.application else {
        return Ok(HelperServices {
            workspace_authority: Arc::new(BrokerAuthority {
                broker,
                observation: workspace.binding.clone(),
                attempt: None,
            }),
            adoption_witness: None,
            application: None,
            workspace_factory: None,
        });
    };
    let context = read_context(&application.host_context)?;
    validate_assignment(&context, spec, application)?;
    let executable = pin_executable(&context)?;
    let authority = Arc::new(BrokerAuthority {
        broker,
        observation: workspace.binding.clone(),
        attempt: Some(context.service.view.attempt.clone()),
    });
    let witness = host::working_copy_witness(&context.service)?;
    Ok(HelperServices {
        workspace_authority: authority.clone(),
        adoption_witness: Some(witness),
        application: Some(Box::new(WorkspaceVcsApplication {
            context,
            assignment: application.clone(),
            authority,
            executable,
            service: None,
        })),
        workspace_factory: None,
    })
}

struct WorkspaceVcsApplication {
    context: WorkspaceVcsContext,
    assignment: HelperApplicationSpec,
    authority: Arc<BrokerAuthority>,
    executable: File,
    service: Option<CommandService>,
}
impl HelperApplication for WorkspaceVcsApplication {
    fn start(&mut self, ready: &WorkspaceRpcReady<'_>) -> Result<GuestServiceProjection> {
        ensure!(
            self.service.is_none()
                && ready.application == &self.assignment
                && ready.binding == &self.context.service.view.observation
                && ready.socket == self.context.service.view.socket
                && ready.identity_file == self.context.service.view.identity_file,
            "workspace VCS deferred callback changed its sealed assignment"
        );
        AttemptAuthority::validate(&*self.authority, ready.binding)?;
        private_directory(&self.context.service.view.projection_state, true)?;
        private_directory(&self.context.service.state_directory, true)?;
        private_directory(&self.assignment.projection_directory, true)?;
        for entry in fs::read_dir(&self.assignment.projection_directory)? {
            let name = entry?.file_name();
            ensure!(
                ["identity.json", "client.json", "jj.sock"]
                    .iter()
                    .any(|allowed| name == *allowed),
                "VCS guest projection contains an unexpected entry"
            );
        }
        host::prepare_workspace(&self.context.service, self.authority.clone())?;
        let mut service_config = self.context.service.clone();
        #[cfg(target_os = "linux")]
        {
            service_config.binary =
                PathBuf::from(format!("/proc/self/fd/{}", self.executable.as_raw_fd()));
        }
        #[cfg(not(target_os = "linux"))]
        {
            bail!("managed workspace VCS requires Linux execution qualification");
        }
        self.service = Some(CommandService::bind(
            service_config,
            &self.assignment.projection_directory,
        )?);
        let guest_directory = self.assignment.guest_directory();
        let client = self.service.as_ref().unwrap().client_config();
        let guest_client = CommandClientConfig {
            format_version: 1,
            socket: PathBuf::from(&guest_directory).join("jj.sock"),
            identity_file: PathBuf::from(&guest_directory).join("identity.json"),
            binding: client.binding,
        };
        write_guest_config(
            &self.assignment.projection_directory.join("client.json"),
            &guest_client,
        )?;
        AttemptAuthority::validate(&*self.authority, ready.binding)?;
        Ok(GuestServiceProjection {
            directory: self.assignment.projection_directory.clone(),
            entries: vec![
                ServiceProjectionEntry {
                    name: "identity.json".into(),
                    node: ServiceProjectionKind::File {
                        max_size_bytes: 32 * 1024,
                    },
                },
                ServiceProjectionEntry {
                    name: "client.json".into(),
                    node: ServiceProjectionKind::File {
                        max_size_bytes: 32 * 1024,
                    },
                },
                ServiceProjectionEntry {
                    name: "jj.sock".into(),
                    node: ServiceProjectionKind::Socket,
                },
            ],
            environment: BTreeMap::from([
                (
                    "RUDDER_JJ_CLIENT_CONFIG".into(),
                    format!("{guest_directory}/client.json"),
                ),
                (
                    "RUDDER_JJ_BIN".into(),
                    self.context.guest_executable.clone(),
                ),
            ]),
        })
    }
    fn close_ingress(&self) {
        if let Some(service) = &self.service {
            service.close();
        }
    }
    fn quiesce(&mut self) -> Result<Box<dyn HelperFinalizationGuard>> {
        self.close_ingress();
        if let Some(service) = &self.service {
            Ok(Box::new(service.quiesce(Duration::from_secs(30))?))
        } else {
            Ok(Box::new(host::fence_private_workspace(
                &self.context.service,
                Duration::from_secs(30),
            )?))
        }
    }
}
impl Drop for WorkspaceVcsApplication {
    fn drop(&mut self) {
        self.close_ingress();
    }
}

fn write_guest_config(path: &Path, value: &impl Serialize) -> Result<()> {
    let directory = path.parent().context("guest projection parent absent")?;
    let outside = directory.parent().context("guest projection base absent")?;
    private_directory(outside, false)?;
    let temporary = outside.join(format!(".vcs-projection-{}", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&temporary)?;
        file.write_all(&canonical_json(value)?)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        File::open(directory)?.sync_all()?;
        File::open(outside)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

#[cfg(test)]
#[path = "workspace_vcs_tests.rs"]
mod tests;
