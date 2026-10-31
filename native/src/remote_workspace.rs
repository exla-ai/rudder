//! Trusted source-only remote factory. Construction runs after execution state
//! reservation, outside the App loop. Context secrets never enter CAS, argv,
//! environment, guest projections, Debug output or authority errors.
use anyhow::{ensure, Context, Result};
use rudder_engine::{
    execution::{
        ExecutionSpec, ExecutionWorkspaceSelectionV4, HelperServices, HelperWorkspaceServices,
        OwnedHelperWorkspace, ProtectedHostContextRef,
    },
    fleet_lease::{HostClock, SuspendAwareClock},
    protocol::canonical_json,
    remote_cache::{CacheLimits, CacheScope, RemoteCache},
    remote_root::{
        AttemptBinding, CurrentAttemptAuthority, RemoteRootRegistry, RemoteSession, RootBinding,
        RootLimits,
    },
    remote_source::{NodeRemoteSource, NodeSourceConfig, SourceRequestControl, TransportProfile},
    store::LocalStore,
    vfs::{
        partial_rpc::{PartialAttemptAuthority, PartialBinding, PROTOCOL},
        remote_launch::{self, RemoteLaunchRequest},
        RemoteOptions,
    },
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
    path::{Component, Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, Weak,
    },
    time::{Duration, Instant},
};

const KIND: &str = "remote-source-only-v1";
const FRESHNESS_NS: u128 = 3_000_000_000;
const MAX_NODE: u64 = 256 * 1024 * 1024;
const MAX_SCRIPT: u64 = 4 * 1024 * 1024;

// Deliberately not Debug or Serialize. The enclosing protected file is private
// selected Rudder fabric configuration, never an OAuth/model profile clone.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HostContext {
    format_version: u32,
    kind: String,
    launch_request_digest: String,
    node: RuntimeFile,
    cli_script: RuntimeFile,
    source: SourceContext,
    cache_root: PathBuf,
    registry_root: PathBuf,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeFile {
    path: PathBuf,
    sha256: String,
    size_bytes: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceContext {
    scope: CacheScope,
    base_url: String,
    auth_headers: BTreeMap<String, String>,
}

fn fault() -> anyhow::Error {
    anyhow::anyhow!("remote workspace authorization unavailable")
}
fn encoded_hex(value: impl AsRef<[u8]>) -> String {
    value
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
fn hex_digest(value: impl AsRef<[u8]>) -> String {
    encoded_hex(Sha256::digest(value.as_ref()))
}
fn normalized(path: &Path) -> bool {
    path.as_os_str().as_bytes().len() <= 4096
        && path.components().count() <= 64
        && path.components().collect::<PathBuf>().as_os_str() == path.as_os_str()
        && path.is_absolute()
        && path != Path::new("/")
        && !path.as_os_str().as_bytes().contains(&0)
        && path
            .components()
            .all(|part| matches!(part, Component::RootDir | Component::Normal(_)))
}
fn digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
fn expected_binding(selection: &RemoteLaunchRequest) -> Result<PartialBinding> {
    selection.validate()?;
    let binding = PartialBinding {
        protocol: PROTOCOL.into(),
        format_version: 1,
        launch_request_digest: hex_digest(canonical_json(selection)?),
        root: selection.root.clone(),
        attempt: selection.attempt.clone(),
        observation: selection.observation.clone(),
    };
    binding.validate()?;
    Ok(binding)
}
fn read_context(
    reference: &ProtectedHostContextRef,
    spec: &ExecutionSpec,
    selection: &RemoteLaunchRequest,
) -> Result<HostContext> {
    let bytes = reference
        .read_bytes()
        .map_err(|_| anyhow::anyhow!("private remote context unavailable"))?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("invalid private remote context"))?;
    // Canonical equality rejects duplicate keys before any typed conversion.
    ensure!(
        canonical_json(&value)? == bytes,
        "private remote context must be canonical"
    );
    let context: HostContext = serde_json::from_value(value)
        .map_err(|_| anyhow::anyhow!("invalid private remote context schema"))?;
    validate_context(&context, spec, selection, TransportProfile::Https)?;
    Ok(context)
}
fn validate_context(
    context: &HostContext,
    spec: &ExecutionSpec,
    selection: &RemoteLaunchRequest,
    transport: TransportProfile,
) -> Result<()> {
    ensure!(
        context.format_version == 1
            && context.kind == KIND
            && context.launch_request_digest == hex_digest(canonical_json(selection)?),
        "remote context selection differs"
    );
    ensure!(
        context.source.scope == selection.root.scope,
        "remote source scope differs"
    );
    context.source.scope.digest()?;
    ensure!(
        normalized(&context.cache_root)
            && normalized(&context.registry_root)
            && !context.cache_root.starts_with(&context.registry_root)
            && !context.registry_root.starts_with(&context.cache_root)
            && [&context.cache_root, &context.registry_root]
                .iter()
                .all(|path| spec
                    .authority_roots
                    .iter()
                    .any(|root| normalized(root) && path.starts_with(root))),
        "remote cache/registry require separate protected roots"
    );
    for (file, max) in [(&context.node, MAX_NODE), (&context.cli_script, MAX_SCRIPT)] {
        ensure!(
            normalized(&file.path)
                && digest(&file.sha256)
                && file.size_bytes > 0
                && file.size_bytes <= max,
            "invalid remote runtime pin"
        );
    }
    let prefix = match transport {
        TransportProfile::Https => "https://",
        TransportProfile::LoopbackHttpTest => "http://",
    };
    ensure!(
        context.source.base_url.starts_with(prefix)
            && context.source.base_url.len() <= 2048
            && !context.source.base_url.contains(['?', '#', '\\'])
            && context.source.base_url.trim() == context.source.base_url,
        "invalid remote source endpoint"
    );
    if matches!(transport, TransportProfile::LoopbackHttpTest) {
        let authority = context
            .source
            .base_url
            .strip_prefix(prefix)
            .unwrap()
            .split('/')
            .next()
            .unwrap();
        ensure!(
            authority.starts_with("127.0.0.1:") || authority.starts_with("[::1]:"),
            "qualification source must be loopback"
        );
    }
    let mut names = std::collections::BTreeSet::new();
    for (name, value) in &context.source.auth_headers {
        let name = name.to_ascii_lowercase();
        ensure!(
            ["authorization", "x-rudder-workspace-id"].contains(&name.as_str())
                && names.insert(name)
                && !value.is_empty()
                && value.len() <= 8192
                && !value.contains(['\r', '\n', '\0']),
            "invalid private fabric header"
        );
    }
    ensure!(
        names.contains("authorization"),
        "private fabric authorization missing"
    );
    Ok(())
}

#[derive(Clone, Copy)]
enum RuntimeTrust {
    Image,
    #[cfg(test)]
    Fixture,
}
fn trusted_owner(uid: u32) -> bool {
    uid == 0
        || fs::read_to_string("/proc/sys/kernel/overflowuid")
            .ok()
            .and_then(|value| value.trim().parse::<u32>().ok())
            == Some(uid)
}
fn open_runtime(path: &Path, trust: RuntimeTrust) -> Result<File> {
    ensure!(normalized(path), "invalid remote runtime path");
    let mut directory = File::open("/")?;
    let parts = path
        .components()
        .filter_map(|part| {
            if let Component::Normal(value) = part {
                Some(value)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    for (index, part) in parts.iter().enumerate() {
        let name = std::ffi::CString::new(part.as_bytes())?;
        let final_part = index + 1 == parts.len();
        let flags = libc::O_RDONLY
            | libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | libc::O_NONBLOCK
            | if final_part { 0 } else { libc::O_DIRECTORY };
        let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
        ensure!(fd >= 0, "protected remote runtime is unavailable");
        let file = unsafe { File::from_raw_fd(fd) };
        let metadata = file.metadata()?;
        if matches!(trust, RuntimeTrust::Image) {
            ensure!(
                trusted_owner(metadata.uid()) && metadata.mode() & 0o022 == 0,
                "remote runtime path is writable by untrusted identity"
            );
        }
        if final_part {
            return Ok(file);
        }
        directory = file;
    }
    Err(anyhow::anyhow!("remote runtime file absent"))
}
#[derive(PartialEq, Eq)]
struct FileIdentity {
    device: u64,
    inode: u64,
    size: u64,
    uid: u32,
    mode: u32,
    modified: (i64, i64),
    changed: (i64, i64),
}
fn identity(file: &File) -> Result<FileIdentity> {
    let m = file.metadata()?;
    ensure!(
        m.is_file() && m.nlink() == 1,
        "remote runtime must be a regular single-link file"
    );
    Ok(FileIdentity {
        device: m.dev(),
        inode: m.ino(),
        size: m.len(),
        uid: m.uid(),
        mode: m.mode(),
        modified: (m.mtime(), m.mtime_nsec()),
        changed: (m.ctime(), m.ctime_nsec()),
    })
}
struct PinnedRuntime {
    file: File,
    path: PathBuf,
    sha256: String,
    identity: FileIdentity,
    trust: RuntimeTrust,
}
impl PinnedRuntime {
    fn open(reference: &RuntimeFile, executable: bool, trust: RuntimeTrust) -> Result<Self> {
        let file = open_runtime(&reference.path, trust)?;
        let observed = identity(&file)?;
        ensure!(
            observed.size == reference.size_bytes && observed.mode & 0o6022 == 0,
            "remote runtime metadata differs"
        );
        if executable {
            ensure!(observed.mode & 0o111 != 0, "remote Node is not executable");
        }
        let mut this = Self {
            file,
            path: reference.path.clone(),
            sha256: reference.sha256.clone(),
            identity: observed,
            trust,
        };
        this.verify()?;
        if executable {
            let mut magic = [0; 4];
            this.file.seek(SeekFrom::Start(0))?;
            this.file.read_exact(&mut magic)?;
            ensure!(&magic == b"\x7fELF", "remote Node must be pinned Linux ELF");
        }
        Ok(this)
    }
    fn verify(&mut self) -> Result<()> {
        ensure!(
            identity(&self.file)? == self.identity,
            "held remote runtime changed"
        );
        self.file.seek(SeekFrom::Start(0))?;
        let mut hash = Sha256::new();
        let mut buffer = [0; 65536];
        let mut size = 0u64;
        loop {
            let count = self.file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            size = size
                .checked_add(count as u64)
                .context("runtime size overflow")?;
            ensure!(size <= self.identity.size, "remote runtime grew");
            hash.update(&buffer[..count]);
        }
        ensure!(
            size == self.identity.size
                && encoded_hex(hash.finalize()) == self.sha256
                && identity(&self.file)? == self.identity,
            "remote runtime hash/identity changed"
        );
        ensure!(
            identity(&open_runtime(&self.path, self.trust)?)? == self.identity,
            "named remote runtime changed"
        );
        Ok(())
    }
}

type Clock = Arc<dyn Fn() -> Result<u128> + Send + Sync>;
struct Freshness {
    last: Option<u128>,
    until: Option<u128>,
    next: u64,
    accepted: u64,
}
struct SessionTracker(Mutex<Option<RemoteSession>>);
struct AuthorityState {
    binding: PartialBinding,
    clock: Clock,
    fenced: AtomicBool,
    freshness: Mutex<Freshness>,
    tracker: Weak<SessionTracker>,
}
struct Authorization {
    serial: u64,
    sent: u128,
}
impl AuthorityState {
    fn new(binding: PartialBinding, clock: Clock, tracker: &Arc<SessionTracker>) -> Arc<Self> {
        Arc::new(Self {
            binding,
            clock,
            fenced: AtomicBool::new(false),
            freshness: Mutex::new(Freshness {
                last: None,
                until: None,
                next: 0,
                accepted: 0,
            }),
            tracker: Arc::downgrade(tracker),
        })
    }
    fn fence(&self) {
        self.fenced.store(true, Ordering::Release);
        if let Some(tracker) = self.tracker.upgrade() {
            if let Ok(session) = tracker.0.lock() {
                if let Some(session) = session.as_ref() {
                    session.invalidate();
                }
            }
        }
    }
    fn sample(&self, state: &mut Freshness, initial: bool) -> Result<u128> {
        ensure!(
            !self.fenced.load(Ordering::Acquire),
            "remote authority fenced"
        );
        let now = (self.clock)()?;
        ensure!(
            state.last.is_none_or(|last| now >= last)
                && state.until.is_none_or(|until| now < until),
            "remote freshness expired or clock regressed"
        );
        ensure!(
            initial || state.until.is_some(),
            "remote authority not initialized"
        );
        state.last = Some(now);
        Ok(now)
    }
    fn begin(&self) -> Result<Authorization> {
        let result: Result<Authorization> = (|| {
            let mut state = self.freshness.lock().map_err(|_| fault())?;
            let sent = self.sample(&mut state, true)?;
            state.next = state
                .next
                .checked_add(1)
                .context("authorization sequence exhausted")?;
            Ok(Authorization {
                serial: state.next,
                sent,
            })
        })();
        if result.is_err() {
            self.fence();
        }
        result.map_err(|_| fault())
    }
    fn accept(&self, request: Authorization) -> Result<()> {
        let result: Result<()> = (|| {
            let mut state = self.freshness.lock().map_err(|_| fault())?;
            let now = self.sample(&mut state, true)?;
            let until = request
                .sent
                .checked_add(FRESHNESS_NS)
                .context("freshness overflow")?;
            ensure!(now < until, "late remote authorization");
            if request.serial > state.accepted {
                state.accepted = request.serial;
                state.until = Some(until);
            }
            Ok(())
        })();
        if result.is_err() {
            self.fence();
        }
        result.map_err(|_| fault())
    }
    fn check(&self) -> Result<()> {
        let result: Result<()> = (|| {
            let mut state = self.freshness.lock().map_err(|_| fault())?;
            self.sample(&mut state, false)?;
            Ok(())
        })();
        if result.is_err() {
            self.fence();
        }
        result.map_err(|_| fault())
    }
    fn register(&self, tracker: &SessionTracker, session: &RemoteSession) -> Result<()> {
        *tracker.0.lock().map_err(|_| fault())? = Some(session.clone());
        if let Err(error) = self.check() {
            session.invalidate();
            return Err(error);
        }
        Ok(())
    }
}
trait BrokerAuthorization: Send + Sync {
    fn authorize(&self, binding: &PartialBinding) -> Result<()>;
}
impl BrokerAuthorization for crate::agent_broker::BrokerClient {
    fn authorize(&self, binding: &PartialBinding) -> Result<()> {
        self.authorize_partial_workspace(binding)
    }
}
struct PartialAuthority {
    broker: Arc<dyn BrokerAuthorization>,
    state: Arc<AuthorityState>,
}
impl PartialAttemptAuthority for PartialAuthority {
    fn validate(&self, binding: &PartialBinding) -> Result<()> {
        if *binding != self.state.binding {
            self.state.fence();
            return Err(fault());
        }
        let request = self.state.begin()?;
        if self.broker.authorize(binding).is_err() {
            self.state.fence();
            return Err(fault());
        }
        self.state.accept(request)
    }
    fn cancelled(&self) -> bool {
        self.state.check().is_err()
    }
}
struct RootAuthority(Arc<AuthorityState>);
impl CurrentAttemptAuthority for RootAuthority {
    fn validate(&self, attempt: &AttemptBinding, root: &RootBinding) -> Result<()> {
        if *attempt != self.0.binding.attempt || *root != self.0.binding.root {
            self.0.fence();
            return Err(fault());
        }
        self.0.check()
    }
}
struct Resources {
    source: Arc<NodeRemoteSource>,
    _cache: RemoteCache,
    _registry: RemoteRootRegistry,
    _tracker: Arc<SessionTracker>,
    _authority: Arc<PartialAuthority>,
    _node: PinnedRuntime,
    _script: PinnedRuntime,
}
impl Drop for Resources {
    fn drop(&mut self) {
        self._authority.state.fence();
        self.source.shutdown();
    }
}

fn private_directory(path: &Path) -> Result<()> {
    ensure!(normalized(path), "invalid protected source directory");
    let mut parent = File::open("/")?;
    for component in path.components().filter_map(|part| {
        if let Component::Normal(part) = part {
            Some(part)
        } else {
            None
        }
    }) {
        let name = std::ffi::CString::new(component.as_bytes())?;
        let mut fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_DIRECTORY,
            )
        };
        if fd < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound {
            let made = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) };
            ensure!(
                made == 0
                    || std::io::Error::last_os_error().kind() == std::io::ErrorKind::AlreadyExists,
                "cannot create protected source directory"
            );
            parent.sync_all()?;
            fd = unsafe {
                libc::openat(
                    parent.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_DIRECTORY,
                )
            };
        }
        ensure!(fd >= 0, "protected source directory unavailable");
        parent = unsafe { File::from_raw_fd(fd) };
    }
    let metadata = parent.metadata()?;
    ensure!(
        metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o777 == 0o700,
        "source directory must be private and owned"
    );
    parent.sync_all()?;
    Ok(())
}
fn open_workspace(
    context: HostContext,
    selection: &RemoteLaunchRequest,
    manager_root: &Path,
    store_root: &Path,
    authority: Arc<PartialAuthority>,
    tracker: Arc<SessionTracker>,
    mut node: PinnedRuntime,
    mut script: PinnedRuntime,
    transport: TransportProfile,
) -> Result<(remote_launch::RemoteLaunchWorkspace, Resources)> {
    authority.validate(&authority.state.binding)?;
    node.verify()?;
    script.verify()?;
    private_directory(&context.cache_root)?;
    private_directory(&context.registry_root)?;
    let mut config = NodeSourceConfig::new(
        context.source.scope,
        context.source.base_url,
        context.source.auth_headers,
    );
    config.transport = transport;
    let source = Arc::new(
        NodeRemoteSource::spawn(&node.path, &script.path, config).map_err(|_| {
            authority.state.fence();
            anyhow::anyhow!("private source startup failed")
        })?,
    );
    // Keep an owned source guard on every later error path; shutdown is a
    // request only and pending admission remains charged until actual settle.
    struct Startup(Option<Arc<NodeRemoteSource>>, Arc<AuthorityState>);
    impl Drop for Startup {
        fn drop(&mut self) {
            if let Some(source) = &self.0 {
                self.1.fence();
                source.shutdown();
            }
        }
    }
    let mut startup = Startup(Some(source.clone()), authority.state.clone());
    node.verify()?;
    script.verify()?;
    authority.state.check()?;
    let cache = RemoteCache::open(&context.cache_root, source.clone(), CacheLimits::default())?;
    let registry = RemoteRootRegistry::open(
        &context.registry_root,
        cache.clone(),
        source.clone(),
        Arc::new(RootAuthority(authority.state.clone())),
        RootLimits::default(),
    )?;
    let session = registry.admit(
        &selection.attempt,
        &selection.root.root_id,
        &selection.root.root,
        &SourceRequestControl::new(Instant::now() + Duration::from_secs(10)),
    )?;
    authority.state.register(&tracker, &session)?;
    let workspace =
        remote_launch::validate_selection(session, selection, RemoteOptions::default())?
            .open_finalizable(manager_root, LocalStore::open(store_root)?)?;
    authority.validate(&authority.state.binding)?;
    let resources = Resources {
        source,
        _cache: cache,
        _registry: registry,
        _tracker: tracker,
        _authority: authority,
        _node: node,
        _script: script,
    };
    // Transfer startup ownership without running the shutdown request.
    drop(startup.0.take());
    Ok((workspace, resources))
}

pub(crate) fn services(spec: &ExecutionSpec) -> Result<HelperServices> {
    #[cfg(not(target_os = "linux"))]
    return Err(anyhow::anyhow!(
        "remote source execution requires qualified Linux"
    ));
    let profile = spec
        .profile
        .as_ref()
        .context("remote execution profile absent")?;
    ensure!(
        spec.format_version == 4
            && profile.format_version == 1
            && profile.network.mode == rudder_engine::execution::network::NetworkMode::OfflineV1,
        "remote source factory requires sealed offline profile4"
    );
    let ExecutionWorkspaceSelectionV4::RemoteSourceOnly {
        selection,
        host_context,
    } = &profile.workspace
    else {
        return Err(anyhow::anyhow!("source-only remote selection required"));
    };
    let workspace = spec
        .workspace_observation
        .as_ref()
        .context("partial workspace projection absent")?;
    let broker_binding = spec
        .broker_binding
        .as_ref()
        .context("partial broker binding absent")?;
    ensure!(
        workspace.application.is_none()
            && selection.observation == workspace.binding
            && profile.request_id == selection.request_id
            && selection.attempt.project_id == broker_binding.project_id
            && selection.attempt.run_id == broker_binding.run_id
            && selection.attempt.run_epoch == broker_binding.run_epoch
            && selection.attempt.execution_id == spec.execution_id
            && selection.attempt.operation_id == spec.operation_id
            && selection.attempt.view_id == spec.view_id
            && selection.attempt.owner == spec.owner,
        "remote source execution binding differs"
    );
    let context = read_context(host_context, spec, selection)?;
    let node = PinnedRuntime::open(&context.node, true, RuntimeTrust::Image)?;
    let script = PinnedRuntime::open(&context.cli_script, false, RuntimeTrust::Image)?;
    let binding = expected_binding(selection)?;
    let expected = crate::agent_broker::BrokerBinding {
        project_id: broker_binding.project_id.clone(),
        run_id: broker_binding.run_id.clone(),
        run_epoch: broker_binding.run_epoch,
        owner_epoch: broker_binding.owner_epoch,
    };
    let directory = spec
        .broker_directory
        .as_ref()
        .context("partial broker directory absent")?;
    let broker = crate::agent_broker::BrokerClient::connect_partial_workspace(
        directory.join("broker.sock"),
        directory.join("identity.json"),
        &expected,
        &binding,
    )
    .map_err(|_| fault())?;
    let tracker = Arc::new(SessionTracker(Mutex::new(None)));
    let host_clock = HostClock::new().map_err(|_| fault())?;
    let clock: Clock = Arc::new(move || {
        host_clock
            .read()
            .map(|value| value.nanoseconds())
            .map_err(|_| fault())
    });
    let state = AuthorityState::new(binding, clock, &tracker);
    let authority = Arc::new(PartialAuthority {
        broker: Arc::new(broker),
        state,
    });
    authority.validate(&authority.state.binding)?;
    let selected = selection.clone();
    let expected_spec = hex_digest(canonical_json(spec)?);
    let for_factory = authority.clone();
    Ok(HelperServices {
        workspace: HelperWorkspaceServices::Partial { authority },
        application: None,
        workspace_factory: Some(Box::new(move |request| {
            ensure!(
                hex_digest(canonical_json(request.spec())?) == expected_spec,
                "remote workspace factory request changed"
            );
            let (workspace, resources) = open_workspace(
                context,
                &selected,
                request.manager_root(),
                request.store_root(),
                for_factory,
                tracker,
                node,
                script,
                TransportProfile::Https,
            )
            .map_err(|_| anyhow::anyhow!("remote source workspace preparation failed"))?;
            OwnedHelperWorkspace::from_remote(request, workspace, resources)
        })),
    })
}

#[cfg(test)]
#[path = "remote_workspace_tests.rs"]
mod tests;
