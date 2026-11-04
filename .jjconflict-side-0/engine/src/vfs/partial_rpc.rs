//! Partial data capture transport for an actual strict remote launch view.
//! This family never returns a complete observation, adopts/retires retention,
//! freezes writers, or proves a checkpoint. Its settled receipt proves only
//! this owned endpoint's completed ingress and calls, never namespace/source
//! transport drain or current owner authority. The outer owner still retains
//! the actual resolver/source lifetime. Close cannot undo an acknowledged ACK.

use super::{
    rpc::{self, CounterGuard, RpcActivity, RpcError, CLIENT_DEADLINE, FRAME_DEADLINE, IO_TIMEOUT},
    DirtyCursor, PartialCaptureBudget, PartialCaptureRequest, VfsView,
};
use crate::{
    protocol::{canonical_json, ObjectKind},
    remote_root::{AttemptBinding, RootBinding},
    store::ImportPolicy,
};
use anyhow::{bail, ensure, Context, Result};
use fs2::FileExt;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::{
        fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use uuid::Uuid;

pub const PROTOCOL: &str = "rudder_partial_workspace_v1";
pub const REQUIRED_OPERATION: &str = "workspace:partial";
pub const SOCKET_NAME: &str = "partial-workspace.sock";
pub const IDENTITY_NAME: &str = "partial-identity.json";
pub const MAX_REQUEST_BYTES: usize = 16 * 1024;
pub const MAX_RESPONSE_BYTES: usize = 256 * 1024;
pub const MAX_DIRTY_EVENTS: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartialBinding {
    pub protocol: String,
    pub format_version: u32,
    /// SHA256 of the whole actual RemoteLaunchRequest, including accepted
    /// source/seed and projection policy. A matching digest is not authority.
    pub launch_request_digest: String,
    pub root: RootBinding,
    pub attempt: AttemptBinding,
    pub observation: rpc::ObservationBinding,
}
impl PartialBinding {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.protocol == PROTOCOL && self.format_version == 1,
            "unsupported partial binding"
        );
        rpc::digest(&self.launch_request_digest)?;
        self.observation.validate()?;
        self.root.scope.digest()?;
        self.root.root.validate()?;
        ensure!(
            self.root.root.kind == ObjectKind::RunnableRoot,
            "partial binding requires runnable root"
        );
        for value in [
            &self.root.root_id,
            &self.attempt.project_id,
            &self.attempt.run_id,
        ] {
            ensure!(
                !value.is_empty() && value.len() <= 512 && !value.chars().any(char::is_control),
                "invalid partial identity"
            );
        }
        ensure!(
            self.attempt.run_epoch > 0
                && self.attempt.execution_id == self.observation.attempt.execution_id
                && self.attempt.operation_id == self.observation.attempt.operation_id
                && self.attempt.view_id == self.observation.view_id
                && self.attempt.owner == self.observation.owner,
            "partial binding differs from exact attempt"
        );
        ensure!(
            self.observation.exclusion_policy_digest
                == super::exclusion_policy_digest(&ImportPolicy::default())?,
            "partial binding requires default source-only exclusions"
        );
        ensure!(
            canonical_json(self)?.len() <= 8 * 1024,
            "partial binding exceeds bound"
        );
        Ok(())
    }
    pub fn digest(&self) -> Result<String> {
        self.validate()?;
        Ok(hex::encode(Sha256::digest(canonical_json(self)?)))
    }
}

/// Trusted host adapters must validate REQUIRED_OPERATION and this exact scope
/// against the current project/run/owner grant. Complete observe scope differs.
pub fn partial_scope(binding: &PartialBinding) -> Result<String> {
    Ok(format!("partial-workspace:{}", binding.digest()?))
}

/// Trusted live authorization, supplied by host ingress. Binding bytes, saved
/// identity files, remote retention and a successful token match are not grants.
/// Validation must be bounded; slow provider/model work belongs in the host's
/// monitor. If a trusted callback does stall, its admission remains charged
/// until it actually returns, even after the caller's deadline or disconnect.
pub trait PartialAttemptAuthority: Send + Sync {
    fn validate(&self, binding: &PartialBinding) -> Result<()>;
    /// Cooperative cancellation only. False never substitutes for validate().
    fn cancelled(&self) -> bool {
        false
    }
}

/// Never Debug-format or log this capability token.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartialIdentity {
    pub protocol: String,
    pub format_version: u32,
    pub binding: PartialBinding,
    pub token: String,
}
impl PartialIdentity {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.protocol == PROTOCOL && self.format_version == 1,
            "unsupported partial identity"
        );
        self.binding.validate()?;
        rpc::digest(&self.token)
    }
}

/// A bounded JSON document carried as a JSON string on the wire. Consumers keep
/// it opaque unless they can preserve full u64 values; a JS Number round trip is
/// not an admissible cursor conversion. This type carries data, never authority.
#[derive(Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct OpaqueJson(String);
impl OpaqueJson {
    pub fn from_serializable(value: &impl Serialize) -> Result<Self> {
        struct Limited(Vec<u8>);
        impl Write for Limited {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if self
                    .0
                    .len()
                    .checked_add(bytes.len())
                    .is_none_or(|size| size > MAX_RESPONSE_BYTES)
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "opaque JSON exceeds bound",
                    ));
                }
                self.0.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut bytes = Limited(Vec::new());
        serde_json::to_writer(&mut bytes, value)?;
        Self::from_json(&bytes.0)
    }
    pub fn from_json(bytes: &[u8]) -> Result<Self> {
        ensure!(
            !bytes.is_empty() && bytes.len() <= MAX_RESPONSE_BYTES,
            "opaque JSON exceeds bound"
        );
        distinct(bytes)?;
        Ok(Self(std::str::from_utf8(bytes)?.into()))
    }
    pub fn as_json(&self) -> &str {
        &self.0
    }
    pub fn decode<T: DeserializeOwned>(&self) -> Result<T> {
        decode(self.0.as_bytes())
    }
}
impl std::fmt::Debug for OpaqueJson {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OpaqueJson")
            .field("bytes", &self.0.len())
            .finish()
    }
}
impl<'de> Deserialize<'de> for OpaqueJson {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let bytes = String::deserialize(deserializer)?;
        Self::from_json(bytes.as_bytes()).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum Operation {
    Cursor,
    Dirty { after: OpaqueJson, limit: usize },
    CapturePartial { request: OpaqueJson },
    InspectPartial { nonce: String },
    PartialHead,
    Cancel { request_id: String },
}
impl<'de> Deserialize<'de> for Operation {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let value = DistinctValue::deserialize(deserializer)?.0;
        let fields = value
            .as_object()
            .ok_or_else(|| serde::de::Error::custom("partial operation must be an object"))?;
        let name = fields
            .get("operation")
            .and_then(|value| value.as_str())
            .ok_or_else(|| serde::de::Error::custom("partial operation required"))?;
        let allowed: &[&str] = match name {
            "cursor" | "partial_head" => &["operation"],
            "dirty" => &["operation", "after", "limit"],
            "capture_partial" => &["operation", "request"],
            "inspect_partial" => &["operation", "nonce"],
            "cancel" => &["operation", "request_id"],
            _ => return Err(serde::de::Error::custom("unsupported partial operation")),
        };
        if fields.len() != allowed.len()
            || fields.keys().any(|key| !allowed.contains(&key.as_str()))
        {
            return Err(serde::de::Error::custom(
                "unknown or missing partial operation field",
            ));
        }
        #[derive(Deserialize)]
        #[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
        enum Wire {
            Cursor,
            Dirty { after: OpaqueJson, limit: usize },
            CapturePartial { request: OpaqueJson },
            InspectPartial { nonce: String },
            PartialHead,
            Cancel { request_id: String },
        }
        Ok(
            match serde_json::from_value(value).map_err(serde::de::Error::custom)? {
                Wire::Cursor => Self::Cursor,
                Wire::Dirty { after, limit } => Self::Dirty { after, limit },
                Wire::CapturePartial { request } => Self::CapturePartial { request },
                Wire::InspectPartial { nonce } => Self::InspectPartial { nonce },
                Wire::PartialHead => Self::PartialHead,
                Wire::Cancel { request_id } => Self::Cancel { request_id },
            },
        )
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Reply {
    Cursor(OpaqueJson),
    Dirty(OpaqueJson),
    CapturePartial(OpaqueJson),
    InspectPartial(OpaqueJson),
    PartialHead(OpaqueJson),
    Cancellation { request_found: bool },
}

#[derive(Debug, Clone)]
pub struct PartialRpcLimits {
    pub max_connections: usize,
    pub max_calls: usize,
    pub capture: PartialCaptureBudget,
}
impl Default for PartialRpcLimits {
    fn default() -> Self {
        Self {
            max_connections: 16,
            max_calls: 2,
            capture: PartialCaptureBudget::default(),
        }
    }
}
impl PartialRpcLimits {
    fn validate(&self) -> Result<()> {
        let work = &self.capture.work;
        let delta = &self.capture.delta;
        ensure!(
            (1..=32).contains(&self.max_connections) && (1..=4).contains(&self.max_calls),
            "invalid partial RPC admission limits"
        );
        ensure!(
            work.max_elapsed_ms > 0
                && work.max_elapsed_ms <= 30_000
                && work.max_changed_entries <= 100_000
                && work.max_content_bytes <= 256 * 1024 * 1024
                && work.max_metadata_bytes <= 128 * 1024 * 1024
                && delta.max_objects > 0
                && delta.max_objects <= 16_000
                && delta.max_boundary_objects > 0
                && delta.max_boundary_objects <= 4096
                && delta.max_metadata_bytes > 0
                && delta.max_metadata_bytes <= 64 * 1024 * 1024
                && !delta.timeout.is_zero()
                && delta.timeout <= Duration::from_secs(30),
            "invalid partial capture work limits"
        );
        Ok(())
    }
}
pub struct PartialRpcConfig {
    pub authority: Arc<dyn PartialAttemptAuthority>,
    pub limits: PartialRpcLimits,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    protocol: String,
    format_version: u32,
    id: String,
    identity: PartialIdentity,
    operation: Operation,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Response {
    protocol: String,
    format_version: u32,
    id: String,
    binding: String,
    result: Option<Reply>,
    error: Option<RpcError>,
}

// Recursive duplicate checking happens before conversion into typed structs,
// including opaque inner cursor/request documents. serde_json::Value alone
// would silently retain the last nested duplicate. Integers stay u64/i64.
struct DistinctValue(serde_json::Value);
impl<'de> Deserialize<'de> for DistinctValue {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = serde_json::Value;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("JSON with distinct object fields")
            }
            fn visit_bool<E: serde::de::Error>(
                self,
                v: bool,
            ) -> std::result::Result<Self::Value, E> {
                Ok(v.into())
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> std::result::Result<Self::Value, E> {
                Ok(v.into())
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> std::result::Result<Self::Value, E> {
                Ok(v.into())
            }
            fn visit_f64<E: serde::de::Error>(self, v: f64) -> std::result::Result<Self::Value, E> {
                serde_json::Number::from_f64(v)
                    .map(serde_json::Value::Number)
                    .ok_or_else(|| E::custom("invalid JSON number"))
            }
            fn visit_str<E: serde::de::Error>(
                self,
                v: &str,
            ) -> std::result::Result<Self::Value, E> {
                Ok(v.into())
            }
            fn visit_string<E: serde::de::Error>(
                self,
                v: String,
            ) -> std::result::Result<Self::Value, E> {
                Ok(v.into())
            }
            fn visit_none<E: serde::de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(serde_json::Value::Null)
            }
            fn visit_unit<E: serde::de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(serde_json::Value::Null)
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut items = Vec::new();
                while let Some(value) = seq.next_element::<DistinctValue>()? {
                    items.push(value.0);
                }
                Ok(items.into())
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut fields = serde_json::Map::new();
                while let Some(key) = map.next_key::<String>()? {
                    if fields.contains_key(&key) {
                        return Err(serde::de::Error::custom("duplicate partial JSON field"));
                    }
                    fields.insert(key, map.next_value::<DistinctValue>()?.0);
                }
                Ok(fields.into())
            }
        }
        deserializer.deserialize_any(Visitor).map(Self)
    }
}
fn distinct(bytes: &[u8]) -> Result<serde_json::Value> {
    Ok(serde_json::from_slice::<DistinctValue>(bytes)?.0)
}
fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    Ok(serde_json::from_value(distinct(bytes)?)?)
}

struct HeldLock {
    file: File,
    path: PathBuf,
    identity: (u64, u64),
}
impl HeldLock {
    fn acquire(path: PathBuf) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)?;
        rpc::private_regular(&file.metadata()?)?;
        FileExt::try_lock_exclusive(&file)
            .context("actual workspace RPC ownership is unavailable")?;
        let metadata = file.metadata()?;
        let this = Self {
            file,
            path,
            identity: (metadata.dev(), metadata.ino()),
        };
        this.check()?;
        Ok(this)
    }
    fn check(&self) -> Result<()> {
        let current = fs::symlink_metadata(&self.path)?;
        rpc::private_regular(&current)?;
        ensure!(
            (current.dev(), current.ino()) == self.identity,
            "partial RPC lock custody changed"
        );
        Ok(())
    }
}
impl Drop for HeldLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

struct Core {
    view: VfsView,
    binding: PartialBinding,
    binding_id: String,
    limits: PartialRpcLimits,
    authority: Arc<dyn PartialAttemptAuthority>,
}
impl Core {
    fn new(view: VfsView, config: PartialRpcConfig) -> Result<Self> {
        config.limits.validate()?;
        let request = view.remote_launch_request()?;
        request.validate()?;
        let binding = PartialBinding {
            protocol: PROTOCOL.into(),
            format_version: 1,
            launch_request_digest: view.remote_launch_digest()?,
            root: request.root.clone(),
            attempt: request.attempt.clone(),
            observation: request.observation.clone(),
        };
        let binding_id = binding.digest()?;
        let this = Self {
            view,
            binding,
            binding_id,
            limits: config.limits,
            authority: config.authority,
        };
        this.validate()?;
        Ok(this)
    }
    fn validate(&self) -> Result<()> {
        self.authority.validate(&self.binding).map_err(|_| {
            rpc::problem(
                "stale_attempt",
                "partial workspace authority is no longer current",
            )
        })?;
        let request = self.view.remote_launch_request()?;
        ensure!(
            request.root == self.binding.root
                && request.attempt == self.binding.attempt
                && request.observation == self.binding.observation
                && self.view.remote_launch_digest()? == self.binding.launch_request_digest,
            "partial workspace launch identity changed"
        );
        let cursor = self.view.dirty_cursor()?;
        self.check_cursor(&cursor)
    }
    fn check_cursor(&self, cursor: &DirtyCursor) -> Result<()> {
        ensure!(
            cursor.view_id == self.binding.observation.view_id
                && cursor.owner == self.binding.observation.owner
                && cursor.namespace == self.binding.observation.namespace,
            "partial cursor belongs to another exact namespace"
        );
        Ok(())
    }
}
struct Inner {
    core: Core,
    identity: PartialIdentity,
    instance: String,
    socket: PathBuf,
    socket_inode: (u64, u64),
    stopped: AtomicBool,
    accept_done: AtomicBool,
    connections: Arc<AtomicUsize>,
    calls: Arc<AtomicUsize>,
    streams: Mutex<BTreeMap<String, UnixStream>>,
    requests: Mutex<BTreeMap<String, Arc<AtomicBool>>>,
    locks: Mutex<Option<(HeldLock, HeldLock)>>,
}
impl Inner {
    fn close(&self) {
        self.stopped.store(true, Ordering::Release);
        if let Ok(requests) = self.requests.lock() {
            for flag in requests.values() {
                flag.store(true, Ordering::Release);
            }
        }
        if let Ok(streams) = self.streams.lock() {
            for stream in streams.values() {
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
        }
    }
    fn authenticate(&self, offered: &PartialIdentity) -> Result<()> {
        ensure!(
            !self.stopped.load(Ordering::Acquire),
            "partial endpoint is closing"
        );
        offered
            .validate()
            .map_err(|_| rpc::problem("unauthenticated", "invalid partial identity"))?;
        let a = offered.token.as_bytes();
        let b = self.identity.token.as_bytes();
        let same =
            a.len() == b.len() && a.iter().zip(b).fold(0u8, |sum, (a, b)| sum | (a ^ b)) == 0;
        if !same || offered.binding != self.core.binding {
            return Err(rpc::problem(
                "unauthenticated",
                "partial capability binding differs",
            ));
        }
        {
            let locks = self
                .locks
                .lock()
                .map_err(|_| rpc::problem("unavailable", "partial lock custody unavailable"))?;
            let (endpoint, view) = locks
                .as_ref()
                .ok_or_else(|| rpc::problem("unavailable", "partial endpoint is settled"))?;
            endpoint.check()?;
            view.check()?;
        }
        self.core.validate()
    }
}

/// Actual endpoint-only settlement. Not Clone/Deserialize and not a saved
/// permission, VFS freeze, process/FUSE drain proof, or checkpoint capability.
#[derive(Debug)]
pub struct PartialRpcSettled {
    binding_digest: String,
    endpoint_instance: String,
}
impl PartialRpcSettled {
    pub fn binding_digest(&self) -> &str {
        &self.binding_digest
    }
    pub fn endpoint_instance(&self) -> &str {
        &self.endpoint_instance
    }
}
pub struct PartialRpc {
    inner: Arc<Inner>,
    accept: Mutex<Option<JoinHandle<()>>>,
}
impl PartialRpc {
    /// Only an actual strict resolver/launch can derive this endpoint binding.
    /// A bind admission lease spans setup, not later nested public capture calls.
    pub fn bind(view: VfsView, config: PartialRpcConfig, directory: &Path) -> Result<Self> {
        let _admission = view.admit_live_effect()?;
        let core = Core::new(view, config)?;
        // EXACT same actual view lock as complete RPC1, despite different wire,
        // socket, identity and token. Separate endpoint paths cannot bypass it.
        let view_lock = HeldLock::acquire(core.view.0.path.join("workspace-rpc.lock"))?;
        let parent = directory
            .parent()
            .context("partial endpoint requires private parent")?
            .canonicalize()?;
        rpc::private_directory(&parent, false)?;
        let directory = parent.join(
            directory
                .file_name()
                .context("invalid partial endpoint name")?,
        );
        rpc::private_directory(&directory, true)?;
        ensure!(
            !directory.starts_with(core.view.manager().store().root()),
            "partial endpoint cannot be in CAS"
        );
        let mut lock_name = directory.file_name().unwrap().to_os_string();
        lock_name.push(".lock");
        let endpoint_lock = HeldLock::acquire(parent.join(lock_name))?;
        let socket = directory.join(SOCKET_NAME);
        rpc::socket_length(&socket)?;
        let identity_path = directory.join(IDENTITY_NAME);
        for entry in fs::read_dir(&directory)? {
            let name = entry?.file_name();
            ensure!(
                name == SOCKET_NAME || name == IDENTITY_NAME,
                "unexpected partial endpoint entry"
            );
        }
        match fs::symlink_metadata(&identity_path) {
            Ok(_) => ensure!(
                read_identity(&identity_path)?.binding == core.binding,
                "partial endpoint belongs to another launch"
            ),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        match fs::symlink_metadata(&socket) {
            Ok(before) => {
                use std::os::unix::fs::FileTypeExt;
                ensure!(
                    before.file_type().is_socket() && before.uid() == unsafe { libc::geteuid() },
                    "invalid partial socket owner/type"
                );
                match rpc::connect_until(&socket, Instant::now() + Duration::from_millis(250)) {
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                        ) => {}
                    _ => bail!("partial socket has live or uncertain listener"),
                }
                let after = fs::symlink_metadata(&socket)?;
                ensure!(
                    (before.dev(), before.ino()) == (after.dev(), after.ino()),
                    "partial socket changed during reconciliation"
                );
                fs::remove_file(&socket)?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        // A restart rotates this family's token; saved identity is never revived.
        let token = hex::encode(Sha256::digest(
            [
                Uuid::new_v4().as_bytes().as_slice(),
                Uuid::new_v4().as_bytes().as_slice(),
            ]
            .concat(),
        ));
        let identity = PartialIdentity {
            protocol: PROTOCOL.into(),
            format_version: 1,
            binding: core.binding.clone(),
            token,
        };
        let temporary = directory.join(format!(".partial-{}.tmp", Uuid::new_v4()));
        let written = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&temporary)?;
            file.write_all(&canonical_json(&identity)?)?;
            file.sync_all()?;
            fs::rename(&temporary, &identity_path)?;
            File::open(&directory)?.sync_all()?;
            Ok::<_, anyhow::Error>(())
        })();
        if written.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        written?;
        core.validate()?;
        view_lock.check()?;
        endpoint_lock.check()?;
        let listener = UnixListener::bind(&socket)?;
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        let metadata = fs::symlink_metadata(&socket)?;
        let inner = Arc::new(Inner {
            core,
            identity,
            instance: Uuid::new_v4().to_string(),
            socket,
            socket_inode: (metadata.dev(), metadata.ino()),
            stopped: AtomicBool::new(false),
            accept_done: AtomicBool::new(false),
            connections: Arc::new(AtomicUsize::new(0)),
            calls: Arc::new(AtomicUsize::new(0)),
            streams: Mutex::new(BTreeMap::new()),
            requests: Mutex::new(BTreeMap::new()),
            locks: Mutex::new(Some((endpoint_lock, view_lock))),
        });
        let running = inner.clone();
        let accept = thread::Builder::new()
            .name("rudder-partial-rpc".into())
            .spawn(move || {
                while !running.stopped.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let guard = match CounterGuard::acquire(
                                &running.connections,
                                running.core.limits.max_connections,
                            ) {
                                Ok(guard) => guard,
                                Err(_) => {
                                    drop(stream);
                                    continue;
                                }
                            };
                            let worker = running.clone();
                            let _ = thread::Builder::new()
                                .name("rudder-partial-rpc-call".into())
                                .spawn(move || {
                                    let _connection = guard;
                                    serve_connection(worker, stream);
                                });
                        }
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5))
                        }
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                        Err(_) => break,
                    }
                }
                drop(listener);
                if fs::symlink_metadata(&running.socket)
                    .is_ok_and(|m| (m.dev(), m.ino()) == running.socket_inode)
                {
                    let _ = fs::remove_file(&running.socket);
                }
                running.accept_done.store(true, Ordering::Release);
            })?;
        Ok(Self {
            inner,
            accept: Mutex::new(Some(accept)),
        })
    }
    pub fn binding(&self) -> &PartialBinding {
        &self.inner.core.binding
    }
    pub fn activity(&self) -> RpcActivity {
        RpcActivity {
            connections: self.inner.connections.load(Ordering::Acquire),
            calls: self.inner.calls.load(Ordering::Acquire),
        }
    }
    pub fn socket_path(&self) -> &Path {
        &self.inner.socket
    }
    pub fn identity_path(&self) -> PathBuf {
        self.inner.socket.with_file_name(IDENTITY_NAME)
    }
    pub fn close(&self) {
        self.inner.close();
    }
    pub fn close_callback(&self) -> Arc<dyn Fn() + Send + Sync> {
        let inner = self.inner.clone();
        Arc::new(move || inner.close())
    }
    pub fn await_closed(&self, timeout: Duration) -> Result<PartialRpcSettled> {
        ensure!(
            timeout <= CLIENT_DEADLINE,
            "partial close wait exceeds bound"
        );
        self.close();
        let deadline = Instant::now() + timeout;
        loop {
            let finished = self
                .accept
                .lock()
                .map_err(|_| rpc::problem("unavailable", "partial accept ownership unavailable"))?
                .as_ref()
                .is_none_or(JoinHandle::is_finished);
            if self.inner.accept_done.load(Ordering::Acquire)
                && finished
                && self.inner.connections.load(Ordering::Acquire) == 0
                && self.inner.calls.load(Ordering::Acquire) == 0
            {
                break;
            }
            if Instant::now() >= deadline {
                return Err(rpc::problem(
                    "deadline",
                    "partial calls are still settling; no drain proof",
                ));
            }
            thread::sleep(Duration::from_millis(2));
        }
        if let Some(accept) = self
            .accept
            .lock()
            .map_err(|_| rpc::problem("unavailable", "partial accept ownership unavailable"))?
            .take()
        {
            accept
                .join()
                .map_err(|_| rpc::problem("unavailable", "partial accept thread failed"))?;
        }
        let mut locks = self
            .inner
            .locks
            .lock()
            .map_err(|_| rpc::problem("unavailable", "partial lock custody unavailable"))?;
        if let Some((endpoint, view)) = locks.as_ref() {
            endpoint.check()?;
            view.check()?;
        }
        locks.take();
        Ok(PartialRpcSettled {
            binding_digest: self.inner.core.binding_id.clone(),
            endpoint_instance: self.inner.instance.clone(),
        })
    }
}
impl Drop for PartialRpc {
    fn drop(&mut self) {
        self.close();
    }
}

fn serve_connection(inner: Arc<Inner>, mut stream: UnixStream) {
    if stream
        .set_nonblocking(false)
        .and_then(|_| stream.set_read_timeout(Some(IO_TIMEOUT)))
        .and_then(|_| stream.set_write_timeout(Some(IO_TIMEOUT)))
        .is_err()
        || !rpc::same_peer_user(&stream)
    {
        return;
    }
    let connection_id = Uuid::new_v4().to_string();
    let copy = match stream.try_clone() {
        Ok(copy) => copy,
        Err(_) => return,
    };
    if let Ok(mut streams) = inner.streams.lock() {
        streams.insert(connection_id.clone(), copy);
    } else {
        return;
    }
    struct Cleanup {
        inner: Arc<Inner>,
        connection: String,
        request: Option<String>,
    }
    impl Drop for Cleanup {
        fn drop(&mut self) {
            if let Ok(mut streams) = self.inner.streams.lock() {
                streams.remove(&self.connection);
            }
            if let Some(id) = &self.request {
                if let Ok(mut requests) = self.inner.requests.lock() {
                    requests.remove(id);
                }
            }
        }
    }
    let mut cleanup = Cleanup {
        inner: inner.clone(),
        connection: connection_id,
        request: None,
    };
    let bytes = match rpc::read_frame(
        &mut stream,
        MAX_REQUEST_BYTES,
        Instant::now() + FRAME_DEADLINE,
        || inner.stopped.load(Ordering::Acquire),
    ) {
        Ok(bytes) => bytes,
        Err(_) => return,
    };
    let request: Request = match decode(&bytes) {
        Ok(request) => request,
        Err(_) => return,
    };
    if request.protocol != PROTOCOL
        || request.format_version != 1
        || rpc::uuid(&request.id).is_err()
    {
        return;
    }
    let result = (|| {
        inner.authenticate(&request.identity)?;
        if let Operation::Cancel { request_id } = &request.operation {
            rpc::uuid(request_id)?;
            let found = {
                let requests = inner.requests.lock().map_err(|_| {
                    rpc::problem("unavailable", "partial request registry unavailable")
                })?;
                if let Some(flag) = requests.get(request_id) {
                    flag.store(true, Ordering::Release);
                    true
                } else {
                    false
                }
            };
            inner.authenticate(&request.identity)?;
            return Ok(Reply::Cancellation {
                request_found: found,
            });
        }
        let _call = CounterGuard::acquire(&inner.calls, inner.core.limits.max_calls)?;
        let cancelled = Arc::new(AtomicBool::new(false));
        {
            let mut requests = inner
                .requests
                .lock()
                .map_err(|_| rpc::problem("unavailable", "partial request registry unavailable"))?;
            ensure!(
                !requests.contains_key(&request.id),
                "duplicate active partial request ID"
            );
            requests.insert(request.id.clone(), cancelled.clone());
            cleanup.request = Some(request.id.clone());
        }
        let deadline =
            Instant::now() + Duration::from_millis(inner.core.limits.capture.work.max_elapsed_ms);
        let stopped = || {
            inner.stopped.load(Ordering::Acquire)
                || cancelled.load(Ordering::Acquire)
                || inner.core.authority.cancelled()
                || Instant::now() >= deadline
                || rpc::disconnected(&stream)
        };
        ensure!(!stopped(), "partial call cancelled before effects");
        inner.core.validate()?;
        ensure!(!stopped(), "partial call cancelled before effects");
        let reply = match &request.operation {
            Operation::Cursor => Reply::Cursor(OpaqueJson::from_serializable(
                &inner.core.view.dirty_cursor()?,
            )?),
            Operation::Dirty { after, limit } => {
                ensure!(
                    *limit > 0 && *limit <= MAX_DIRTY_EVENTS,
                    "partial dirty page exceeds bound"
                );
                let after: DirtyCursor = after.decode()?;
                inner.core.check_cursor(&after)?;
                Reply::Dirty(OpaqueJson::from_serializable(
                    &inner.core.view.dirty_since(&after, *limit)?,
                )?)
            }
            Operation::CapturePartial { request } => {
                let capture: PartialCaptureRequest = request.decode()?;
                inner.core.check_cursor(&capture.expected_begin)?;
                let receipt = inner.core.view.capture_partial_source_cancellable(
                    &ImportPolicy::default(),
                    &capture,
                    &inner.core.limits.capture,
                    &stopped,
                )?;
                Reply::CapturePartial(OpaqueJson::from_serializable(&receipt)?)
            }
            Operation::InspectPartial { nonce } => {
                rpc::uuid(nonce)?;
                Reply::InspectPartial(OpaqueJson::from_serializable(
                    &inner.core.view.inspect_partial_capture(nonce)?,
                )?)
            }
            Operation::PartialHead => Reply::PartialHead(OpaqueJson::from_serializable(
                &inner.core.view.partial_capture_head()?,
            )?),
            Operation::Cancel { .. } => unreachable!(),
        };
        ensure!(
            !stopped(),
            "partial call cancelled; reconcile scoped receipt before retry"
        );
        inner.authenticate(&request.identity)?;
        Ok(reply)
    })();
    let (result,error) = match result {
        Ok(reply) => (Some(reply),None),
        Err(error) => (None,Some(error.downcast_ref::<RpcError>().cloned().unwrap_or_else(|| RpcError {
            code: "operation_failed".into(), message: "partial operation failed; reconcile its scoped data receipt before retry".into() }))),
    };
    let response = Response {
        protocol: PROTOCOL.into(),
        format_version: 1,
        id: request.id,
        binding: inner.core.binding_id.clone(),
        result,
        error,
    };
    let _ = rpc::write_frame(
        &mut stream,
        &response,
        MAX_RESPONSE_BYTES,
        Instant::now() + FRAME_DEADLINE,
        || inner.stopped.load(Ordering::Acquire),
    );
}

#[derive(Clone)]
pub struct PartialClient {
    socket: PathBuf,
    identity: PathBuf,
    binding: PartialBinding,
}
impl PartialClient {
    pub fn new(
        socket: impl Into<PathBuf>,
        identity: impl Into<PathBuf>,
        binding: PartialBinding,
    ) -> Result<Self> {
        binding.validate()?;
        let this = Self {
            socket: socket.into(),
            identity: identity.into(),
            binding,
        };
        rpc::socket_length(&this.socket)?;
        ensure!(
            read_identity(&this.identity)?.binding == this.binding,
            "partial identity differs from configured binding"
        );
        Ok(this)
    }
    pub fn call(&self, operation: Operation) -> Result<Reply> {
        self.call_with_cancel(operation, &AtomicBool::new(false))
    }
    pub fn call_with_cancel(&self, operation: Operation, cancelled: &AtomicBool) -> Result<Reply> {
        self.call_identified(&Uuid::new_v4().to_string(), operation, cancelled)
    }
    pub fn call_identified(
        &self,
        id: &str,
        operation: Operation,
        cancelled: &AtomicBool,
    ) -> Result<Reply> {
        rpc::uuid(id)?;
        ensure!(
            !cancelled.load(Ordering::Acquire),
            "partial caller cancelled before dispatch"
        );
        let identity = read_identity(&self.identity)?;
        ensure!(
            identity.binding == self.binding,
            "partial identity belongs to another exact launch"
        );
        let mut stream = rpc::connect_until(&self.socket, Instant::now() + FRAME_DEADLINE)?;
        stream.set_read_timeout(Some(IO_TIMEOUT))?;
        stream.set_write_timeout(Some(IO_TIMEOUT))?;
        rpc::write_frame(
            &mut stream,
            &Request {
                protocol: PROTOCOL.into(),
                format_version: 1,
                id: id.into(),
                identity,
                operation,
            },
            MAX_REQUEST_BYTES,
            Instant::now() + FRAME_DEADLINE,
            || cancelled.load(Ordering::Acquire),
        )?;
        let bytes = rpc::read_frame(
            &mut stream,
            MAX_RESPONSE_BYTES,
            Instant::now() + CLIENT_DEADLINE,
            || cancelled.load(Ordering::Acquire),
        )?;
        let response: Response = decode(&bytes)?;
        ensure!(
            response.protocol == PROTOCOL
                && response.format_version == 1
                && response.id == id
                && response.binding == self.binding.digest()?
                && response.result.is_some() != response.error.is_some(),
            "partial response binding invalid"
        );
        if let Some(error) = response.error {
            return Err(error.into());
        }
        Ok(response.result.unwrap())
    }
}

pub fn read_identity(path: &Path) -> Result<PartialIdentity> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)?;
    let before = file.metadata()?;
    rpc::private_regular(&before)?;
    ensure!(
        before.len() <= MAX_REQUEST_BYTES as u64,
        "partial identity exceeds bound"
    );
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(MAX_REQUEST_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    let after = file.metadata()?;
    ensure!(
        bytes.len() as u64 == before.len()
            && before.len() == after.len()
            && (before.dev(), before.ino()) == (after.dev(), after.ino()),
        "partial identity changed while reading"
    );
    let identity: PartialIdentity = decode(&bytes)?;
    identity.validate()?;
    Ok(identity)
}
