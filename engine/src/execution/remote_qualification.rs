//! Explicit disposable Spec4 qualification, never a production services factory.
//!
//! The source and authority below are synthetic, immutable, credential-free and
//! outside the guest. A trusted in-memory receipt models remote admission; it is
//! not evidence of a provider upload, fleet grant or accepted jj history. The
//! success path nevertheless uses the actual sealed managed handoff, namespace
//! and capability readiness, FUSE, authenticated stop, frozen capture and State
//! Record2 finalizer. No serialized value reconstructs those owned witnesses.

use super::{
    handoff::{self, ManagedExecutionEvent, ManagedExecutionHandle, ManagedExecutionHandoff},
    network::{
        NetworkBinding, NetworkBudgets, NetworkEpoch, NetworkExecutionProfile, NetworkMode,
        NetworkProfileV1,
    },
    read_execution_recovery, BrokerBinding, CheckpointRef, ExecutionOutcome, ExecutionProfileV4,
    ExecutionRecord, ExecutionSpec, ExecutionWorkspaceSelectionV4, HelperServices,
    HelperServicesFactory, HelperWorkspaceServices, ManagedExecutionBinding, ManagedFrontendStart,
    OwnedHelperWorkspace, ProtectedHostContextRef, ReadOnlyRuntime, StopAndCheckpoint,
    WorkspaceObservationProfile,
};
use crate::{
    agent_api::AgentIdentityFile,
    content::{self, FileNode, CHUNK_BYTES, FILE_LAYOUT},
    delta,
    directory::{
        self, DirectoryEntry, DirectoryStore, EntryNode, LocalDirectoryStore, PagedDirectory,
    },
    protocol::{canonical_json, CheckpointConsistency, ObjectKind, ObjectRef, OwnerEpoch},
    remote_cache::{CacheLimits, CacheScope, FetchControl, ImmutableObjectSource, RemoteCache},
    remote_root::{
        AttemptBinding, CurrentAttemptAuthority, RemoteRootRegistry, RootBinding, RootLimits,
        RootReceiptSource,
    },
    remote_source::{
        ReplicatedDurability, ReplicatedRootReceipt, SourceError, SourceRequestControl,
    },
    snapshot::{RunnableRoot, SourceV2},
    store::{ImportPolicy, LocalStore},
    vfs::{
        exclusion_policy_digest,
        partial_rpc::{PartialAttemptAuthority, PartialBinding},
        remote_launch::{self, RemoteLaunchRequest},
        rpc::{AttemptContext, ObservationBinding},
        ObservationConsistency, RemoteOptions,
    },
    workspace::WorkspaceProfile,
};
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{
            fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
            process::CommandExt,
        },
    },
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

const KIND: &str = "remote-execution-qualification";
const MAX_ENTRIES: usize = 100_101;
const LOG_LIMIT: usize = 64 * 1024;
const LOG_FAIL_LIMIT: u64 = 1024 * 1024;
const SOURCE_BYTE_LIMIT: u64 = 64 * 1024 * 1024;
const SOURCE_CALL_LIMIT: u64 = 4096;
const FILE_LOGICAL_BYTES: u64 = 52 * CHUNK_BYTES as u64;
const CASE_TIMEOUT: Duration = Duration::from_secs(180);
const READY_NAME: &str = "remote-guest-ready.json";
const NEW_FILE: &str = "qualified-private.txt";

// The marker is synchronization DATA only. BackendReady3 is independently
// required and validated by the real managed receiver. Both processes retain
// the same mapped file until the actual namespace/PID-tree stop kills them.
const GUEST: &str = r#"import json,mmap,os,socket,sys,time
request,entries,logical,private_json=sys.argv[1:]
entries=int(entries); logical=int(logical)
assert os.getuid()==1500 and os.getgid()==1500
assert not os.getgroups()
status={}
with open('/proc/self/status') as stream:
    for line in stream:
        key,_,value=line.partition(':'); status[key]=value.strip()
assert all(int(status[key],16)==0 for key in ['CapEff','CapPrm','CapInh','CapAmb','CapBnd'])
assert status['NoNewPrivs']=='1'
interfaces=sorted(name for _,name in socket.if_nameindex())
assert interfaces==['lo']
private=json.loads(private_json)
assert all(not os.path.exists(path) for path in private)
assert not os.path.exists('/dev/fuse')
with os.scandir('/workspace') as stream:
    count=sum(1 for entry in stream if entry.name.startswith('file-'))
assert count==entries
assert os.stat('/workspace/file-00000000').st_size==logical
data=b'small-new-file:'+request.encode()+b'\n'
fd=os.open('/workspace/qualified-private.txt',os.O_RDWR|os.O_CREAT|os.O_EXCL,0o600)
assert os.write(fd,data)==len(data)
mapped=mmap.mmap(fd,len(data)); mapped[0:1]=b'Q'; mapped.flush(); os.fsync(fd)
descendant=os.fork()
if descendant==0:
    while True: time.sleep(1)
marker={'request_id':request,'entries':count,'logical_bytes':logical,'open_fd':True,
        'mmap_open':True,'descendant_started':descendant>0,'zero_capabilities':True,
        'no_new_privileges':True,'interfaces':interfaces,'private_paths_absent':True}
out=os.open('/scratch/remote-guest-ready.json',os.O_WRONLY|os.O_CREAT|os.O_EXCL,0o600)
raw=json.dumps(marker,sort_keys=True,separators=(',',':')).encode()
assert os.write(out,raw)==len(raw); os.fsync(out); os.close(out)
directory=os.open('/scratch',os.O_RDONLY|os.O_DIRECTORY); os.fsync(directory); os.close(directory)
while True: time.sleep(1)
"#;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HostContext {
    format_version: u32,
    fixture_id: String,
    root: PathBuf,
    entries: usize,
}
impl HostContext {
    fn validate(&self) -> Result<()> {
        ensure!(self.format_version == 1, "unsupported synthetic context");
        ensure!(
            Uuid::parse_str(&self.fixture_id)?.to_string() == self.fixture_id,
            "invalid synthetic fixture identity"
        );
        ensure!(
            (1..=MAX_ENTRIES).contains(&self.entries),
            "fixture count exceeds bound"
        );
        ensure!(
            self.root.is_absolute() && self.root.canonicalize()? == self.root,
            "fixture root must be canonical"
        );
        private_metadata(&self.root, true)?;
        Ok(())
    }
    fn scope(&self) -> CacheScope {
        CacheScope {
            provider_id: "synthetic-offline-execution-v1".into(),
            tenant_id: self.fixture_id.clone(),
        }
    }
}

#[derive(Clone, Default, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceStats {
    root_admissions: u64,
    fetch_calls: u64,
    fetched_bytes: u64,
    file_metadata_requests: u64,
    unchanged_payload_requests: u64,
    accepted_seed_requests: u64,
    active_fetches: u64,
    maximum_active_fetches: u64,
}
#[derive(Clone)]
struct GraphRefs {
    payload: ObjectRef,
    file: ObjectRef,
    directory: ObjectRef,
    seed: ObjectRef,
    profile: ObjectRef,
    policy: ObjectRef,
    source: ObjectRef,
    root: ObjectRef,
}
struct SyntheticSource {
    scope: CacheScope,
    objects: Mutex<BTreeMap<ObjectRef, Vec<u8>>>,
    refs: Mutex<Option<GraphRefs>>,
    stats: Mutex<SourceStats>,
}
impl SyntheticSource {
    fn put_bytes(&self, kind: ObjectKind, bytes: &[u8]) -> Result<ObjectRef> {
        let reference = ObjectRef::from_bytes(kind, bytes);
        let mut objects = self.objects.lock().unwrap();
        let total: u64 = objects.values().map(|bytes| bytes.len() as u64).sum();
        ensure!(
            total + bytes.len() as u64 <= SOURCE_BYTE_LIMIT && objects.len() < 4096,
            "synthetic immutable graph exceeds bound"
        );
        objects.insert(reference.clone(), bytes.to_vec());
        Ok(reference)
    }
    fn metadata(&self, kind: ObjectKind, value: &impl Serialize) -> Result<ObjectRef> {
        self.put_bytes(kind, &canonical_json(value)?)
    }
    fn build(context: &HostContext) -> Result<(Arc<Self>, GraphRefs)> {
        ensure!(
            (1..=MAX_ENTRIES).contains(&context.entries),
            "fixture count exceeds bound"
        );
        let source = Arc::new(Self {
            scope: context.scope(),
            objects: Mutex::new(BTreeMap::new()),
            refs: Mutex::new(None),
            stats: Mutex::new(SourceStats::default()),
        });
        let payload = source.put_bytes(ObjectKind::Blob, &vec![b'p'; CHUNK_BYTES])?;
        let file = source.metadata(
            ObjectKind::File,
            &FileNode {
                format_version: 1,
                layout: FILE_LAYOUT.into(),
                level: 0,
                logical_size: FILE_LOGICAL_BYTES,
                children: vec![payload.clone(); 52],
            },
        )?;
        let entries = (0..context.entries)
            .map(|index| {
                DirectoryEntry::new(
                    format!("file-{index:08}").as_bytes(),
                    EntryNode::File {
                        object: file.clone(),
                        logical_size: FILE_LOGICAL_BYTES,
                        mode: 0o644,
                    },
                )
            })
            .collect::<Result<Vec<_>>>()?;
        let directory = PagedDirectory::new(source.as_ref()).build(0o755, entries)?;
        let seed = PagedDirectory::new(source.as_ref()).build(0o700, [])?;
        let profile = source.metadata(ObjectKind::Blob, &WorkspaceProfile::source_only())?;
        let policy = source.put_bytes(ObjectKind::Blob, b"{\"format_version\":1}")?;
        let accepted_source = source.metadata(
            ObjectKind::SourceRoot,
            &SourceV2 {
                format_version: 2,
                consistency: CheckpointConsistency::Quiesced,
                directory: directory.clone(),
                projection_policy: policy.clone(),
            },
        )?;
        let hardlinks =
            source.metadata(ObjectKind::Blob, &json!({"format_version":1,"groups":[]}))?;
        let root = source.metadata(
            ObjectKind::RunnableRoot,
            &RunnableRoot {
                format_version: 1,
                consistency: CheckpointConsistency::Quiesced,
                source: accepted_source.clone(),
                environment: None,
                view_root: directory.clone(),
                profile: profile.clone(),
                hardlinks,
                knowledge_cursor: 0,
            },
        )?;
        let refs = GraphRefs {
            payload,
            file,
            directory,
            seed,
            profile,
            policy,
            source: accepted_source,
            root,
        };
        *source.refs.lock().unwrap() = Some(refs.clone());
        Ok((source, refs))
    }
    fn snapshot(&self) -> SourceStats {
        self.stats.lock().unwrap().clone()
    }
    fn receipt(&self) -> ReplicatedRootReceipt {
        let objects = self.objects.lock().unwrap();
        ReplicatedRootReceipt {
            format_version: 1,
            root_id: "synthetic-source-only-root".into(),
            root: self.refs.lock().unwrap().as_ref().unwrap().root.clone(),
            durability: ReplicatedDurability::Replicated,
            published_at_ms: 1,
            object_count: objects.len() as u64,
            total_bytes: objects.values().map(|bytes| bytes.len() as u64).sum(),
        }
    }
}
impl DirectoryStore for SyntheticSource {
    fn read(&self, reference: &ObjectRef) -> Result<Vec<u8>> {
        self.objects
            .lock()
            .unwrap()
            .get(reference)
            .cloned()
            .context("synthetic object absent")
    }
    fn put(&self, kind: ObjectKind, bytes: &[u8]) -> Result<ObjectRef> {
        directory::references_from_bytes(kind, bytes)?;
        self.put_bytes(kind, bytes)
    }
}
struct FetchLease<'a>(&'a SyntheticSource);
impl Drop for FetchLease<'_> {
    fn drop(&mut self) {
        self.0.stats.lock().unwrap().active_fetches -= 1;
    }
}
impl ImmutableObjectSource for SyntheticSource {
    fn scope(&self) -> CacheScope {
        self.scope.clone()
    }
    fn fetch(
        &self,
        reference: &ObjectRef,
        control: &FetchControl,
        output: &mut dyn Write,
    ) -> Result<()> {
        let refs = self.refs.lock().unwrap().as_ref().unwrap().clone();
        {
            let mut stats = self.stats.lock().unwrap();
            stats.fetch_calls += 1;
            if reference == &refs.file {
                stats.file_metadata_requests += 1;
            }
            if reference == &refs.payload {
                stats.unchanged_payload_requests += 1;
            }
            if reference == &refs.seed {
                stats.accepted_seed_requests += 1;
            }
            ensure!(
                stats.fetch_calls <= SOURCE_CALL_LIMIT,
                "source call budget exhausted"
            );
            ensure!(
                reference != &refs.payload && reference != &refs.seed,
                "unchanged payload or private seed hydration forbidden"
            );
            stats.active_fetches += 1;
            stats.maximum_active_fetches = stats.maximum_active_fetches.max(stats.active_fetches);
        }
        let _lease = FetchLease(self);
        let objects = self.objects.lock().unwrap();
        let bytes = objects
            .get(reference)
            .context("synthetic source object absent")?;
        for chunk in bytes.chunks(64 * 1024) {
            control.check()?;
            {
                let mut stats = self.stats.lock().unwrap();
                ensure!(
                    stats.fetched_bytes + chunk.len() as u64 <= SOURCE_BYTE_LIMIT,
                    "source byte budget exhausted"
                );
                stats.fetched_bytes += chunk.len() as u64;
            }
            output.write_all(chunk)?;
        }
        Ok(())
    }
}
impl RootReceiptSource for SyntheticSource {
    fn scope(&self) -> CacheScope {
        self.scope.clone()
    }
    fn get_root(
        &self,
        root_id: &str,
        _: &SourceRequestControl,
    ) -> std::result::Result<ReplicatedRootReceipt, SourceError> {
        self.stats.lock().unwrap().root_admissions += 1;
        let receipt = self.receipt();
        if root_id != receipt.root_id {
            return Err(SourceError::RemoteFailed);
        }
        Ok(receipt)
    }
}

struct SyntheticAuthority {
    binding: PartialBinding,
    revoked: AtomicBool,
}
impl CurrentAttemptAuthority for SyntheticAuthority {
    fn validate(&self, attempt: &AttemptBinding, root: &RootBinding) -> Result<()> {
        ensure!(
            !self.revoked.load(Ordering::Acquire)
                && attempt == &self.binding.attempt
                && root == &self.binding.root,
            "synthetic source grant differs or is revoked"
        );
        Ok(())
    }
}
impl PartialAttemptAuthority for SyntheticAuthority {
    fn validate(&self, binding: &PartialBinding) -> Result<()> {
        ensure!(
            !self.revoked.load(Ordering::Acquire) && binding == &self.binding,
            "synthetic partial grant differs or is revoked"
        );
        Ok(())
    }
    fn cancelled(&self) -> bool {
        self.revoked.load(Ordering::Acquire)
    }
}

fn private_metadata(path: &Path, directory: bool) -> Result<fs::Metadata> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        !metadata.file_type().is_symlink()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0
            && if directory {
                metadata.is_dir()
            } else {
                metadata.is_file() && metadata.nlink() == 1
            },
        "unsafe private fixture path"
    );
    Ok(metadata)
}
fn mkdir(path: &Path) -> Result<()> {
    fs::create_dir(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    File::open(path.parent().context("fixture directory has no parent")?)?.sync_all()?;
    Ok(())
}
fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    ensure!(
        bytes.len() <= 256 * 1024,
        "private fixture file exceeds bound"
    );
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    File::open(path.parent().context("fixture file has no parent")?)?.sync_all()?;
    Ok(())
}
fn read_private(path: &Path, max: usize) -> Result<Vec<u8>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.nlink() == 1
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0
            && metadata.len() <= max as u64,
        "unsafe or oversized fixture read"
    );
    let mut bytes = Vec::new();
    file.take(max as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= max, "fixture read exceeded bound");
    let current = fs::symlink_metadata(path)?;
    ensure!(
        current.dev() == metadata.dev()
            && current.ino() == metadata.ino()
            && current.is_file()
            && !current.file_type().is_symlink(),
        "fixture file changed"
    );
    Ok(bytes)
}
fn sha(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}
fn utc() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let value = seconds as libc::time_t;
    let mut out = std::mem::MaybeUninit::<libc::tm>::uninit();
    if unsafe { libc::gmtime_r(&value, out.as_mut_ptr()) }.is_null() {
        return seconds.to_string();
    }
    let value = unsafe { out.assume_init() };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        value.tm_year + 1900,
        value.tm_mon + 1,
        value.tm_mday,
        value.tm_hour,
        value.tm_min,
        value.tm_sec
    )
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HelperReport {
    kind: String,
    outcome: String,
    record_sha256: String,
    source: Option<SourceStats>,
}
fn services_factory(observed: Arc<Mutex<Option<Arc<SyntheticSource>>>>) -> HelperServicesFactory {
    // No source, cache worker or authority monitor is started until this
    // closure is called by the held new-execution State reservation.
    Box::new(move |spec| {
        let profile = spec
            .profile
            .as_ref()
            .context("qualifier requires profile4")?;
        let ExecutionWorkspaceSelectionV4::RemoteSourceOnly {
            selection,
            host_context,
        } = &profile.workspace
        else {
            bail!("qualifier requires source-only remote selection")
        };
        let context: HostContext = serde_json::from_slice(&host_context.read_bytes()?)?;
        context.validate()?;
        ensure!(
            spec.manager_root == context.root.join("manager")
                && spec.store_root == context.root.join("store")
                && spec.execution_root == context.root.join("executions")
                && selection.root.scope == context.scope(),
            "qualifier context differs from sealed spec"
        );
        write_new(&context.root.join("authority/factory-called"), b"1\n")?;
        let binding = profile
            .partial_binding()?
            .context("partial binding absent")?;
        let authority = Arc::new(SyntheticAuthority {
            binding,
            revoked: AtomicBool::new(false),
        });
        let expected = selection.clone();
        let source_slot = observed.clone();
        Ok(HelperServices {
            workspace: HelperWorkspaceServices::Partial {
                authority: authority.clone(),
            },
            application: None,
            workspace_factory: Some(Box::new(move |new| {
                let (source, refs) = SyntheticSource::build(&context)?;
                ensure!(
                    refs.root == expected.root.root
                        && refs.source == expected.accepted_source
                        && refs.seed == expected.accepted_seed
                        && refs.policy == expected.projection_policy
                        && refs.profile == expected.observation.profile,
                    "synthetic immutable graph differs from sealed selection"
                );
                let cache = RemoteCache::open(
                    context.root.join("cache"),
                    source.clone(),
                    CacheLimits::default(),
                )?;
                let registry = RemoteRootRegistry::open(
                    context.root.join("registry"),
                    cache.clone(),
                    source.clone(),
                    authority.clone(),
                    RootLimits::default(),
                )?;
                let session = registry.admit(
                    &expected.attempt,
                    &expected.root.root_id,
                    &expected.root.root,
                    &SourceRequestControl::new(Instant::now() + Duration::from_secs(5)),
                )?;
                let workspace = remote_launch::validate_selection(
                    session,
                    &expected,
                    RemoteOptions::default(),
                )?
                .open_finalizable(new.manager_root(), LocalStore::open(new.store_root())?)?;
                ensure!(
                    source.snapshot().unchanged_payload_requests == 0
                        && source.snapshot().accepted_seed_requests == 0,
                    "pre-mount hydration occurred"
                );
                *source_slot.lock().unwrap() = Some(source.clone());
                // Both opaque view/manager and this owner retain the actual
                // session source/cache/registry/authority through State fsync.
                OwnedHelperWorkspace::from_remote(
                    new,
                    workspace,
                    (source, cache, registry, authority),
                )
            })),
        })
    })
}

fn helper(name: &str, parent: u32) -> Result<i32> {
    let (spec, control) = handoff::receive_managed(name, parent).context("managed-handoff")?;
    let diagnostic_root = spec
        .execution_root
        .parent()
        .context("qualifier execution root has no parent")?
        .to_path_buf();
    private_metadata(&diagnostic_root, true)?;
    let result = (|| -> Result<i32> {
        let observed = Arc::new(Mutex::new(None));
        let outcome = super::linux::run_helper_with_managed_services(
            spec,
            services_factory(observed.clone()),
            control,
        )
        .context("managed-helper-run")?;
        let report = HelperReport {
            kind: "remote-execution-helper-outcome".into(),
            outcome: match &outcome {
                ExecutionOutcome::Finalized(_) => "finalized",
                ExecutionOutcome::Historical(_) => "historical",
            }
            .into(),
            record_sha256: sha(&canonical_json(outcome.record())?),
            source: observed
                .lock()
                .unwrap()
                .as_ref()
                .map(|source| source.snapshot()),
        };
        if let Some(finalized) = outcome.finalized() {
            ensure!(
                finalized.isolation().network_isolated,
                "finalized result lacks network isolation"
            );
            ensure!(
                matches!(
                    finalized.record().checkpoint(),
                    Some(CheckpointRef::PartialRemote(_))
                ),
                "qualifier returned a complete checkpoint"
            );
        }
        println!("{}", serde_json::to_string(&report)?);
        // Keep the owned final result alive through its fixed report. Historical
        // data never enters finalized() and cannot recreate a physical witness.
        drop(outcome);
        Ok(0)
    })();
    match result {
        Ok(code) => Ok(code),
        // Preserve the original synthetic-only chain in the sealed fixture's
        // private root. Public output still contains only fixed labels/hashes.
        Err(error) => Ok(emit_failure(
            "managed-helper-run",
            &error,
            Some(&diagnostic_root),
        )),
    }
}

#[derive(Default)]
struct Captured {
    bytes: Vec<u8>,
    count: u64,
    digest: String,
}
struct PipeCapture {
    data: Arc<Mutex<Captured>>,
    done: mpsc::Receiver<()>,
    join: Option<thread::JoinHandle<()>>,
}
impl PipeCapture {
    fn start(mut input: impl Read + Send + 'static) -> Self {
        let data = Arc::new(Mutex::new(Captured::default()));
        let target = data.clone();
        let (send, done) = mpsc::sync_channel(1);
        let join = thread::spawn(move || {
            let mut hasher = Sha256::new();
            let mut buffer = [0; 8192];
            loop {
                match input.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => {
                        hasher.update(&buffer[..count]);
                        let mut data = target.lock().unwrap();
                        data.count += count as u64;
                        let keep = count.min(LOG_LIMIT.saturating_sub(data.bytes.len()));
                        data.bytes.extend_from_slice(&buffer[..keep]);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            target.lock().unwrap().digest = hex::encode(hasher.finalize());
            let _ = send.send(());
        });
        Self {
            data,
            done,
            join: Some(join),
        }
    }
    fn settle(&mut self) -> Result<()> {
        self.done
            .recv_timeout(Duration::from_secs(3))
            .context("helper pipe settlement deadline")?;
        self.join
            .take()
            .unwrap()
            .join()
            .map_err(|_| anyhow::anyhow!("helper pipe reader panicked"))?;
        Ok(())
    }
    fn exceeded(&self) -> bool {
        self.data.lock().unwrap().count > LOG_FAIL_LIMIT
    }
    fn save(&self, path: &Path) -> Result<Value> {
        let captured = self.data.lock().unwrap();
        write_new(path, &captured.bytes)?;
        Ok(
            json!({"bytes":captured.count,"sha256":captured.digest,"retained_bytes":captured.bytes.len(),
            "truncated":captured.count>captured.bytes.len() as u64}),
        )
    }
    fn report(&self) -> Result<HelperReport> {
        let captured = self.data.lock().unwrap();
        ensure!(
            captured.count <= LOG_LIMIT as u64,
            "helper stdout exceeded fixed protocol bound"
        );
        let mut report = None;
        for line in captured
            .bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
        {
            if let Ok(value) = serde_json::from_slice::<HelperReport>(line) {
                ensure!(
                    value.kind == "remote-execution-helper-outcome" && report.is_none(),
                    "duplicate or unknown helper result"
                );
                report = Some(value);
            } else {
                bail!("helper stdout is outside fixed result protocol");
            }
        }
        report.context("helper fixed outcome absent")
    }
}

/// Only this actual unreaped leader reserves the process-group identity. No
/// try_wait/reap occurs before group cleanup, including the normal exit path.
struct OwnedHelper {
    child: Child,
    pidfd: OwnedFd,
    stdout: PipeCapture,
    stderr: PipeCapture,
    reaped: bool,
}
impl OwnedHelper {
    fn spawn(args: &[String], root: &Path) -> Result<Self> {
        let parent = std::process::id();
        let mut command = Command::new(std::env::current_exe()?);
        command
            .args(args)
            .current_dir(root)
            .env_clear()
            .env("HOME", root.join("host-home"))
            .env("RUDDER_HOME", root.join("host-home/rudder"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        unsafe {
            command.pre_exec(move || {
                if libc::setsid() < 0
                    || libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0
                    || libc::getppid() as u32 != parent
                {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 4u32) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().context("spawn-owned-helper")?;
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, child.id(), 0) };
        if raw < 0 {
            // The owned child has not been reaped; its group ID is reserved.
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            let _ = child.kill();
            let _ = child.wait();
            bail!("helper pidfd unavailable");
        }
        Ok(Self {
            pidfd: unsafe { OwnedFd::from_raw_fd(raw as i32) },
            stdout: PipeCapture::start(child.stdout.take().context("helper stdout absent")?),
            stderr: PipeCapture::start(child.stderr.take().context("helper stderr absent")?),
            child,
            reaped: false,
        })
    }
    fn exited(&self) -> Result<bool> {
        let mut fd = libc::pollfd {
            fd: self.pidfd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut fd, 1, 0) };
        if result < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(result > 0 && fd.revents & libc::POLLIN != 0)
    }
    fn bounded(&self) -> Result<()> {
        ensure!(
            !self.stdout.exceeded() && !self.stderr.exceeded(),
            "helper log byte budget exhausted"
        );
        Ok(())
    }
    fn finish(&mut self, deadline: Instant) -> Result<ExitStatus> {
        while !self.exited()? {
            ensure!(Instant::now() < deadline, "helper exit deadline");
            self.bounded()?;
            thread::sleep(Duration::from_millis(10));
        }
        // A zombie remains owned until wait(), so killpg cannot hit a recycled
        // leader. Actual guest-tree drain is proved by the typed receipt, not
        // by this best-effort disposable runner cleanup.
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
        }
        let status = self.child.wait()?;
        self.reaped = true;
        self.stdout.settle()?;
        self.stderr.settle()?;
        self.bounded()?;
        Ok(status)
    }
}
impl Drop for OwnedHelper {
    fn drop(&mut self) {
        if self.reaped {
            return;
        }
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.pidfd.as_raw_fd(),
                libc::SIGKILL,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            );
        }
        let end = Instant::now() + Duration::from_secs(3);
        while !self.exited().unwrap_or(false) && Instant::now() < end {
            thread::sleep(Duration::from_millis(10));
        }
        if self.exited().unwrap_or(false) {
            let _ = self.child.wait();
            self.reaped = true;
        }
    }
}

fn selection(context: &HostContext, refs: &GraphRefs) -> Result<RemoteLaunchRequest> {
    let attempt = AttemptBinding {
        project_id: "remote-execution-qualification".into(),
        run_id: "synthetic-offline-run".into(),
        run_epoch: 1,
        owner: OwnerEpoch {
            owner: "synthetic-offline-owner".into(),
            epoch: 1,
        },
        execution_id: Uuid::new_v4().to_string(),
        operation_id: Uuid::new_v4().to_string(),
        view_id: Uuid::new_v4().to_string(),
    };
    let request = RemoteLaunchRequest {
        format_version: 1,
        request_id: Uuid::new_v4().to_string(),
        root: RootBinding {
            scope: context.scope(),
            root_id: "synthetic-source-only-root".into(),
            root: refs.root.clone(),
        },
        observation: ObservationBinding {
            format_version: 1,
            attempt: AttemptContext {
                execution_id: attempt.execution_id.clone(),
                operation_id: attempt.operation_id.clone(),
            },
            view_id: attempt.view_id.clone(),
            owner: attempt.owner.clone(),
            namespace: Uuid::new_v4().to_string(),
            profile: refs.profile.clone(),
            exclusion_policy_digest: exclusion_policy_digest(&ImportPolicy::default())?,
        },
        attempt,
        projection_policy: refs.policy.clone(),
        accepted_source: refs.source.clone(),
        accepted_seed: refs.seed.clone(),
    };
    request.validate()?;
    Ok(request)
}

fn execution_spec(
    context: &HostContext,
    request: RemoteLaunchRequest,
) -> Result<(ExecutionSpec, ManagedExecutionBinding)> {
    let authority = context.root.join("authority");
    let bytes = canonical_json(context)?;
    let context_path = authority.join("synthetic-context.json");
    write_new(&context_path, &bytes)?;
    let attempt = &request.attempt;
    let network = NetworkProfileV1 {
        format_version: 1,
        execution_profile: NetworkExecutionProfile::CloudUid1500LinuxV1,
        binding: NetworkBinding {
            project_id: attempt.project_id.clone(),
            owner: attempt.owner.owner.clone(),
            owner_epoch: NetworkEpoch::new(attempt.owner.epoch)?,
            run_id: attempt.run_id.clone(),
            run_epoch: NetworkEpoch::new(attempt.run_epoch)?,
            request_id: request.request_id.clone(),
            execution_id: attempt.execution_id.clone(),
            operation_id: attempt.operation_id.clone(),
            view_id: attempt.view_id.clone(),
            network_id: Uuid::new_v4().to_string(),
        },
        mode: NetworkMode::OfflineV1,
        policy: None,
        policy_sha256: None,
        budgets: NetworkBudgets {
            max_connections: 0,
            max_dns_jobs: 0,
            max_request_bytes: 0,
            max_buffer_bytes: 0,
            max_bytes_per_connection: 0,
            max_total_bytes: 0,
            connect_timeout_ms: 0,
            idle_timeout_ms: 0,
            total_timeout_ms: 0,
        },
        readiness_nonce: Uuid::new_v4().to_string(),
    };
    let profile = ExecutionProfileV4 {
        format_version: 1,
        startup: None,
        request_id: request.request_id.clone(),
        network,
        workspace: ExecutionWorkspaceSelectionV4::RemoteSourceOnly {
            selection: request.clone(),
            host_context: ProtectedHostContextRef {
                format_version: 1,
                path: context_path,
                sha256: sha(&bytes),
                size_bytes: bytes.len() as u64,
            },
        },
    };
    // A nonempty synthetic broker credential satisfies the same projection
    // schema; no broker server, real Journal capability or provider auth is used.
    let identity = AgentIdentityFile {
        format_version: 1,
        project_id: attempt.project_id.clone(),
        run_id: attempt.run_id.clone(),
        run_epoch: attempt.run_epoch,
        owner_epoch: attempt.owner.epoch,
        token: format!("synthetic-{}", Uuid::new_v4()),
    };
    write_new(
        &context.root.join("agent/identity.json"),
        &canonical_json(&identity)?,
    )?;
    let private = vec![
        context.root.join("authority"),
        context.root.join("cache"),
        context.root.join("registry"),
        context.root.join("manager"),
        context.root.join("store"),
    ];
    let mut runtime = vec![ReadOnlyRuntime {
        source: "/usr".into(),
        guest_path: "/usr".into(),
    }];
    if Path::new("/etc/ld.so.cache").is_file() {
        runtime.push(ReadOnlyRuntime {
            source: "/etc/ld.so.cache".into(),
            guest_path: "/etc/ld.so.cache".into(),
        });
    }
    let spec = ExecutionSpec {
        format_version: 4,
        execution_id: attempt.execution_id.clone(),
        operation_id: attempt.operation_id.clone(),
        execution_root: context.root.join("executions"),
        manager_root: context.root.join("manager"),
        store_root: context.root.join("store"),
        authority_roots: vec![
            authority,
            context.root.join("cache"),
            context.root.join("registry"),
        ],
        view_id: attempt.view_id.clone(),
        owner: attempt.owner.clone(),
        command: vec![
            "/usr/bin/python3".into(),
            "-I".into(),
            "-c".into(),
            GUEST.into(),
            request.request_id.clone(),
            context.entries.to_string(),
            FILE_LOGICAL_BYTES.to_string(),
            serde_json::to_string(&private)?,
        ],
        environment: BTreeMap::new(),
        runtime,
        home_files: Vec::new(),
        broker_directory: Some(context.root.join("agent")),
        broker_binding: Some(BrokerBinding {
            project_id: attempt.project_id.clone(),
            run_id: attempt.run_id.clone(),
            run_epoch: attempt.run_epoch,
            owner_epoch: attempt.owner.epoch,
        }),
        workspace_observation: Some(WorkspaceObservationProfile {
            binding: request.observation.clone(),
            endpoint_directory: context.root.join("rpc"),
            application: None,
        }),
        profile: Some(profile.clone()),
        timeout_seconds: CASE_TIMEOUT.as_secs(),
    };
    let binding = ManagedExecutionBinding {
        format_version: 3,
        request_id: request.request_id.clone(),
        run_id: attempt.run_id.clone(),
        run_epoch: attempt.run_epoch,
        execution_id: attempt.execution_id.clone(),
        operation_id: attempt.operation_id.clone(),
        view_id: attempt.view_id.clone(),
        owner: attempt.owner.clone(),
        execution_profile: "offline-command-v1".into(),
        frontend_start: ManagedFrontendStart::Immediate,
        server_command: None,
        guest_socket: "/tmp/cx.sock".into(),
        profile: Some(profile),
    };
    spec.validate()?;
    binding.validate(&spec)?;
    Ok((spec, binding))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GuestReady {
    request_id: String,
    entries: usize,
    logical_bytes: u64,
    open_fd: bool,
    mmap_open: bool,
    descendant_started: bool,
    zero_capabilities: bool,
    no_new_privileges: bool,
    interfaces: Vec<String>,
    private_paths_absent: bool,
}
fn ready_marker(path: &Path, request: &RemoteLaunchRequest, entries: usize) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
        Ok(_) => {}
    }
    // The writer creates then fsyncs the marker. A short intermediate JSON
    // prefix is not readiness and may be retried within the same deadline.
    let bytes = read_private(path, 4096)?;
    let marker: GuestReady = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(_) => return Ok(false),
    };
    ensure!(
        marker.request_id == request.request_id
            && marker.entries == entries
            && marker.logical_bytes == FILE_LOGICAL_BYTES
            && marker.open_fd
            && marker.mmap_open
            && marker.descendant_started
            && marker.zero_capabilities
            && marker.no_new_privileges
            && marker.interfaces == ["lo"]
            && marker.private_paths_absent,
        "guest marker does not describe the requested isolated mapped writer"
    );
    Ok(true)
}

fn await_ready(
    helper: &OwnedHelper,
    handle: &ManagedExecutionHandle,
    spec: &ExecutionSpec,
    binding: &ManagedExecutionBinding,
    request: &RemoteLaunchRequest,
    entries: usize,
    deadline: Instant,
) -> Result<()> {
    let mut backend = None;
    let marker = spec
        .execution_root
        .join("runs")
        .join(&spec.execution_id)
        .join("scratch")
        .join(READY_NAME);
    loop {
        ensure!(Instant::now() < deadline, "guest readiness deadline");
        helper.bounded()?;
        while let Some(event) = handle.try_event() {
            match event {
                ManagedExecutionEvent::BackendReady { process } => {
                    ensure!(
                        backend.is_none()
                            && process.matches_attempt(
                                &binding.run_id,
                                &binding.execution_id,
                                "offline-command-v1"
                            )
                            && !process.authenticated_start_required(),
                        "backend readiness belongs to another profile or attempt"
                    );
                    process.check_current()?;
                    backend = Some(process);
                }
                ManagedExecutionEvent::Shutdown(_) => {
                    bail!("shutdown arrived before an authenticated stop")
                }
                ManagedExecutionEvent::SourceShutdown(_) => {
                    bail!("source shutdown arrived in a binding3 qualification")
                }
                ManagedExecutionEvent::Uncertain => {
                    bail!("managed helper readiness became uncertain")
                }
            }
        }
        if let Some(process) = &backend {
            if ready_marker(&marker, request, entries)? {
                process.check_current()?;
                ensure!(!helper.exited()?, "helper exited before authenticated stop");
                return Ok(());
            }
        }
        ensure!(!helper.exited()?, "helper exited before guest readiness");
        thread::sleep(Duration::from_millis(10));
    }
}
fn await_shutdown(
    helper: &OwnedHelper,
    handle: &ManagedExecutionHandle,
    stop: &StopAndCheckpoint,
    deadline: Instant,
) -> Result<ExecutionRecord> {
    loop {
        ensure!(Instant::now() < deadline, "authenticated shutdown deadline");
        helper.bounded()?;
        while let Some(event) = handle.try_event() {
            match event {
                ManagedExecutionEvent::Shutdown(receipt) => {
                    ensure!(
                        receipt.request() == stop
                            && receipt.record().format_version() == 2
                            && matches!(
                                receipt.record().checkpoint(),
                                Some(CheckpointRef::PartialRemote(_))
                            ),
                        "shutdown is not the exact typed partial Record2 result"
                    );
                    return Ok(receipt.record().clone());
                }
                ManagedExecutionEvent::BackendReady { .. } => {
                    bail!("backend readiness repeated after stop")
                }
                ManagedExecutionEvent::Uncertain => bail!("managed shutdown became uncertain"),
                ManagedExecutionEvent::SourceShutdown(_) => {
                    bail!("binding3 qualification cannot adopt provisional source shutdown")
                }
            }
        }
        // The real receiver drains a final frame after pidfd readiness. Do not
        // race that receiver by treating helper exit alone as a missing receipt.
        thread::sleep(Duration::from_millis(10));
    }
}

fn verify_data(
    spec: &ExecutionSpec,
    request: &RemoteLaunchRequest,
    refs: &GraphRefs,
    stop: &StopAndCheckpoint,
    record: &ExecutionRecord,
) -> Result<Value> {
    record.validate_metadata()?;
    let Some(CheckpointRef::PartialRemote(checkpoint)) = record.checkpoint() else {
        bail!("partial artifact absent")
    };
    ensure!(
        checkpoint.stop == *stop
            && checkpoint.observation == request.observation
            && checkpoint.execution_request_digest == sha(&canonical_json(spec)?)
            && checkpoint.capture.request_nonce == stop.request_id
            && checkpoint.capture.base == request.root
            && checkpoint.capture.attempt == request.attempt
            && checkpoint.capture.consistency == ObservationConsistency::LiveBestEffort,
        "partial artifact changed its immutable selection or live-capture format"
    );
    let store = LocalStore::open(&spec.store_root)?;
    ensure!(
        store.retention()?.pins.is_empty(),
        "remote execution created a complete local pin"
    );
    ensure!(
        !store.object_path(&refs.payload)?.try_exists()?
            && !store.object_path(&refs.seed)?.try_exists()?,
        "unchanged payload or accepted seed was imported into local CAS"
    );
    let plan = delta::read_partial_tree(&store, &checkpoint.capture.retention)?;
    ensure!(
        plan.base == request.root
            && plan.attempt == request.attempt
            && !plan.remote_boundaries.is_empty(),
        "capture lost its authorized inherited boundary"
    );
    ensure!(
        !plan.local_objects.contains(&refs.payload) && !plan.local_objects.contains(&refs.file),
        "capture retained unchanged File or payload as producer data"
    );
    let directories = LocalDirectoryStore::read_only(&store);
    let entry = PagedDirectory::new(&directories)
        .lookup(&plan.directory, NEW_FILE.as_bytes())?
        .context("guest write absent from retained partial tree")?;
    let EntryNode::File {
        object,
        logical_size,
        ..
    } = entry.node
    else {
        bail!("guest write changed kind")
    };
    let mut expected = format!("small-new-file:{}\n", request.request_id).into_bytes();
    expected[0] = b'Q';
    ensure!(
        logical_size == expected.len() as u64
            && content::read_file_range(&store, &object, 0, expected.len() + 1)? == expected,
        "mmap bytes differ from the physically drained guest write"
    );
    let evidence = store.object_set_refs(&checkpoint.evidence_retention)?;
    ensure!(
        evidence.len() == 2
            && evidence.contains(&checkpoint.freeze_intent)
            && evidence.contains(&checkpoint.drain_record),
        "exact frozen/drain evidence retention absent"
    );
    let freeze = store.read_object(&checkpoint.freeze_intent)?;
    let drain = store.read_object(&checkpoint.drain_record)?;
    let local_objects = store.object_set_refs(&checkpoint.capture.retention.local)?;
    let gc = store.gc(Duration::ZERO)?;
    ensure!(
        !gc.blocked_by_staging,
        "GC qualification is blocked by a surviving staging lease"
    );
    for object in local_objects.iter().chain(evidence.iter()) {
        store.read_object(object)?;
    }
    ensure!(
        delta::read_partial_tree(&store, &checkpoint.capture.retention)? == plan
            && store.retention()?.pins.is_empty(),
        "GC altered retained data or complete pin semantics"
    );
    Ok(
        json!({"record_format":2,"artifact":"partial_remote","capture_format":checkpoint.capture.format_version,
        "capture_consistency":"live_best_effort","stop_nonce":stop.request_id,
        "partial_directory":plan.directory,"local_objects":local_objects.len(),
        "producer_objects":plan.local_objects.len(),"remote_boundaries":plan.remote_boundaries.len(),
        "new_file_bytes":expected.len(),"new_file_sha256":sha(&expected),
        "freeze_intent":checkpoint.freeze_intent,"freeze_bytes_sha256":sha(&freeze),
        "drain_record":checkpoint.drain_record,"drain_bytes_sha256":sha(&drain),
        "capture_retention":checkpoint.capture.retention,"evidence_retention":checkpoint.evidence_retention,
        "complete_pins":0,"gc":gc}),
    )
}

fn run_case(context: &HostContext) -> Result<Value> {
    context.validate()?;
    let began = Instant::now();
    let (source, refs) = SyntheticSource::build(context).context("build-synthetic-graph")?;
    let graph_receipt = source.receipt();
    ensure!(
        source.snapshot().fetch_calls == 0,
        "fixture construction used transport reads"
    );
    let request = selection(context, &refs)?;
    // Parent drops its graph before helper launch. Only the helper's newly
    // admitted session can supply inherited bytes; delta CAS starts empty.
    drop(source);
    let (spec, binding) = execution_spec(context, request.clone()).context("build-sealed-spec")?;
    let handoff =
        ManagedExecutionHandoff::new(&spec, binding.clone()).context("prepare-managed-handoff")?;
    let mut helper = OwnedHelper::spawn(&handoff.helper_args(), &context.root)?;
    let (transferred, handle) = handoff.activate(helper.child.id())?;
    transferred
        .wait(Duration::from_secs(12))
        .context("managed-transfer")?;
    let deadline = Instant::now() + CASE_TIMEOUT;
    let result = (|| -> Result<_> {
        await_ready(
            &helper,
            &handle,
            &spec,
            &binding,
            &request,
            context.entries,
            deadline,
        )?;
        let stop = StopAndCheckpoint {
            request_id: Uuid::new_v4().to_string(),
            execution_id: spec.execution_id.clone(),
            operation_id: spec.operation_id.clone(),
            view_id: spec.view_id.clone(),
            run_id: binding.run_id.clone(),
            run_epoch: binding.run_epoch,
            owner: binding.owner.clone(),
        };
        handle.stop_and_checkpoint(stop.clone())?;
        handle.stop_and_checkpoint(stop.clone())?; // exact idempotent controller retry
        let record = await_shutdown(&helper, &handle, &stop, deadline)?;
        let status = helper.finish(deadline)?;
        ensure!(
            status.success(),
            "helper returned a failing process status after receipt"
        );
        Ok((stop, record))
    })();
    if result.is_err() {
        // Preserve only our private, bounded synthetic fixture output. No
        // public report emits argv, credentials, raw stderr or guest content.
        drop(handle);
        if !helper.reaped {
            unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    helper.pidfd.as_raw_fd(),
                    libc::SIGKILL,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                );
            }
            let _ = helper.finish(Instant::now() + Duration::from_secs(5));
        }
        let _ = helper.stdout.save(&context.root.join("initial.stdout"));
        let _ = helper.stderr.save(&context.root.join("initial.stderr"));
    }
    let (stop, record) = result.context("physical-remote-execution")?;
    let stdout = helper.stdout.save(&context.root.join("initial.stdout"))?;
    let stderr = helper.stderr.save(&context.root.join("initial.stderr"))?;
    let report = helper.stdout.report()?;
    ensure!(
        report.outcome == "finalized" && report.record_sha256 == sha(&canonical_json(&record)?),
        "helper result differs from typed shutdown record"
    );
    let stats = report
        .source
        .context("new execution has no actual source counters")?;
    ensure!(
        stats.root_admissions == 1
            && stats.fetch_calls > 0
            && stats.active_fetches == 0
            && stats.unchanged_payload_requests == 0
            && stats.accepted_seed_requests == 0
            && stats.fetch_calls <= SOURCE_CALL_LIMIT
            && stats.fetched_bytes <= SOURCE_BYTE_LIMIT,
        "actual remote source counters exceed source-only bounds"
    );
    let data = verify_data(&spec, &request, &refs, &stop, &record)
        .context("verify-retained-partial-data")?;
    let historical = read_execution_recovery(&spec.execution_root, &spec.execution_id)?
        .context("persisted restart record absent")?;
    ensure!(
        canonical_json(historical.record())? == canonical_json(&record)?
            && historical.remote_evidence().is_some(),
        "persisted historical data differs from live result"
    );
    // This is a new actual helper/process/handoff with the same immutable
    // request. State must return historical data before either factory starts.
    let reopen = ManagedExecutionHandoff::new(&spec, binding)?;
    let mut retry = OwnedHelper::spawn(&reopen.helper_args(), &context.root)?;
    let (transferred, reopened) = reopen.activate(retry.child.id())?;
    transferred.wait(Duration::from_secs(12))?;
    let retry_status = retry.finish(Instant::now() + Duration::from_secs(15))?;
    ensure!(retry_status.success(), "historical helper process failed");
    while let Some(event) = reopened.try_event() {
        ensure!(
            !matches!(
                event,
                ManagedExecutionEvent::BackendReady { .. } | ManagedExecutionEvent::Shutdown(_)
                    | ManagedExecutionEvent::SourceShutdown(_)
            ),
            "historical retry emitted current process or shutdown authority"
        );
    }
    let retry_stdout = retry.stdout.save(&context.root.join("historical.stdout"))?;
    let retry_stderr = retry.stderr.save(&context.root.join("historical.stderr"))?;
    let retry_report = retry.stdout.report()?;
    ensure!(
        retry_report.outcome == "historical"
            && retry_report.source.is_none()
            && retry_report.record_sha256 == report.record_sha256
            && read_private(&context.root.join("authority/factory-called"), 4)? == b"1\n",
        "historical retry reopened the remote source or changed recorded data"
    );
    let after = read_execution_recovery(&spec.execution_root, &spec.execution_id)?
        .context("historical record disappeared")?;
    ensure!(
        canonical_json(after.record())? == canonical_json(&record)?,
        "retry altered the terminal record"
    );
    let groups = [
        json!({"kind":KIND,"format_version":1,"status":"passed","utc":utc(),
        "fixture_root":context.root,"execution_id":spec.execution_id,"request_id":request.request_id,
        "execution_spec":4,"managed_binding":3,"network_readiness_protocol":3,"namespace_schema":5}),
        json!({
        "profile":"offline-command-v1","network":"actual_private_offline_namespace",
        "io":"owned_pipes","pty_qualified":false,"synthetic_authority":true,"provider_upload_qualified":false,
        "source_only":true,"environment_free":true,"empty_topology":true,"vcs_application":false}),
        json!({
        "entries":context.entries,"inherited_file_logical_bytes":FILE_LOGICAL_BYTES,
        "synthetic_graph_objects":graph_receipt.object_count,"synthetic_graph_bytes":graph_receipt.total_bytes,
        "inherited_directory":refs.directory,"inherited_file":refs.file,"inherited_payload":refs.payload,
        "source":stats,"data":data}),
        json!({"backend_ready_authenticated":true,"guest_marker_checked":true,
        "mmap_and_open_fd_and_descendant":true,"shutdown_typed_and_authenticated":true,
        "record_sha256":report.record_sha256,"historical_retry":{"outcome":"historical","factory_calls":1,
            "new_source":false,"current_shutdown_authority":false,"stdout":retry_stdout,"stderr":retry_stderr},
        "stdout":stdout,"stderr":stderr,"elapsed_ms":began.elapsed().as_millis(),"limits":limits()}),
    ];
    let mut proof = serde_json::Map::new();
    for group in groups {
        let Value::Object(fields) = group else {
            unreachable!("literal proof object")
        };
        for (key, value) in fields {
            ensure!(proof.insert(key, value).is_none(), "duplicate proof field");
        }
    }
    Ok(Value::Object(proof))
}

fn limits() -> Value {
    json!({"max_entries":MAX_ENTRIES,"case_timeout_seconds":CASE_TIMEOUT.as_secs(),"process_alarm_seconds":240,
        "source_fetch_calls":SOURCE_CALL_LIMIT,"source_fetched_bytes":SOURCE_BYTE_LIMIT,
        "log_retained_bytes_per_pipe":LOG_LIMIT,"log_failure_bytes_per_pipe":LOG_FAIL_LIMIT,
        "capture_budget":"unchanged default producer and delta limits","remote_receipt":"synthetic immutable source only",
        "complete_pin_or_acceptance":false,"host_fallback":false})
}
fn prepare_identity() -> Result<()> {
    if unsafe { libc::getuid() } == 0 {
        ensure!(
            unsafe { libc::setgroups(0, std::ptr::null()) } == 0
                && unsafe { libc::setresgid(1500, 1500, 1500) } == 0
                && unsafe { libc::setresuid(1500, 1500, 1500) } == 0,
            "Uid1500Unavailable"
        );
        // Fresh /proc ownership after the saved-UID transition, as in the
        // actual worker. No profile/environment or pre-created fixture moves.
        let error = Command::new(std::env::current_exe()?)
            .args(std::env::args().skip(1))
            .env_clear()
            .exec();
        return Err(error).context("Uid1500ReexecFailed");
    }
    ensure!(
        unsafe { libc::getuid() } == 1500
            && unsafe { libc::geteuid() } == 1500
            && unsafe { libc::getgid() } == 1500
            && unsafe { libc::getegid() } == 1500,
        "Uid1500Required"
    );
    ensure!(
        unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1 as libc::c_ulong, 0, 0, 0) } == 0,
        "NoNewPrivilegesUnavailable"
    );
    Ok(())
}

/// No command is run without explicit `qualify`; `--describe` is read-only.
/// All diagnostics exposed on stdout are bounded hashes/static stage labels.
pub fn run_cli() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args == ["--describe"] {
        println!(
            "{}",
            json!({"kind":KIND,"status":"description","implemented_case":"offline_source_only_actual_managed_stop",
            "usage":"qualify --base ABSOLUTE_PRIVATE_PARENT [--entries 1..100101]","limits":limits()})
        );
        return 0;
    }
    unsafe {
        libc::umask(0o077);
        libc::alarm(240);
    }
    if args == ["fabric", "execution", "child"] {
        return match super::linux::run_child() {
            Ok(()) => 0,
            Err(error) => emit_failure("restricted-child", &error, None),
        };
    }
    if args.len() == 7
        && args[..3] == ["fabric", "execution", "managed-helper"]
        && args[3] == "--handoff"
        && args[5] == "--parent-pid"
    {
        return match args[6]
            .parse::<u32>()
            .context("invalid helper parent")
            .and_then(|parent| helper(&args[4], parent))
        {
            Ok(code) => code,
            Err(error) => emit_failure("managed-helper", &error, None),
        };
    }
    let parsed = (|| -> Result<(PathBuf, usize)> {
        ensure!(
            args.first().map(String::as_str) == Some("qualify"),
            "ExplicitQualifyRequired"
        );
        let mut base = PathBuf::from("/workspace");
        let mut entries = MAX_ENTRIES;
        let mut index = 1;
        while index < args.len() {
            let value = args
                .get(index + 1)
                .context("missing qualifier argument value")?;
            match args[index].as_str() {
                "--base" => base = value.into(),
                "--entries" => entries = value.parse()?,
                _ => bail!("unknown qualifier argument"),
            }
            index += 2;
        }
        ensure!(
            (1..=MAX_ENTRIES).contains(&entries),
            "fixture count exceeds bound"
        );
        ensure!(base.is_absolute(), "qualifier base must be absolute");
        Ok((base, entries))
    })();
    let (base, entries) = match parsed {
        Ok(value) => value,
        Err(error) => return emit_failure("arguments", &error, None),
    };
    if let Err(error) = prepare_identity() {
        return emit_failure("identity", &error, None);
    }
    let setup = (|| -> Result<HostContext> {
        ensure!(
            Path::new("/usr/bin/python3").is_file(),
            "PublicPythonRuntimeUnavailable"
        );
        ensure!(Path::new("/dev/fuse").exists(), "FuseUnavailable");
        let base = base.canonicalize()?;
        let id = Uuid::new_v4().to_string();
        let root = base.join(format!("rreq-{}", &id[..8]));
        ensure!(
            root.join("rpc/partial-workspace.sock")
                .as_os_str()
                .as_encoded_bytes()
                .len()
                < 100,
            "qualifier base exceeds Unix socket bound"
        );
        mkdir(&root)?;
        for name in ["authority", "cache", "registry", "agent", "host-home"] {
            mkdir(&root.join(name))?;
        }
        let context = HostContext {
            format_version: 1,
            fixture_id: id,
            root,
            entries,
        };
        context.validate()?;
        Ok(context)
    })();
    let context = match setup {
        Ok(value) => value,
        Err(error) => return emit_failure("fixture-setup", &error, None),
    };
    match run_case(&context) {
        Ok(proof) => {
            if let Err(error) = write_new(
                &context.root.join("proof.json"),
                &canonical_json(&proof).unwrap(),
            ) {
                return emit_failure("save-proof", &error, Some(&context.root));
            }
            println!("{proof}");
            0
        }
        Err(error) => emit_failure("qualify", &error, Some(&context.root)),
    }
}
fn emit_failure(stage: &str, error: &anyhow::Error, root: Option<&Path>) -> i32 {
    let detail = format!("{error:#}");
    // Private synthetic-only diagnostics remain inspectable without printing
    // arbitrary errors/argv to provider logs. They contain no real profiles.
    if let Some(root) = root {
        let _ = write_new(
            &root.join("failure.private.txt"),
            &detail.as_bytes()[..detail.len().min(64 * 1024)],
        );
    }
    let unsupported = matches!(stage, "identity")
        || matches!(
            error.to_string().as_str(),
            "PublicPythonRuntimeUnavailable" | "FuseUnavailable"
        );
    let errno = error.chain().find_map(|error| {
        error
            .downcast_ref::<std::io::Error>()
            .and_then(|error| error.raw_os_error())
    });
    println!(
        "{}",
        json!({"kind":KIND,"status":if unsupported {"unsupported"} else {"failed"},"utc":utc(),
        "stage":stage,"error_sha256":sha(detail.as_bytes()),"error_bytes":detail.len(),"errno":errno,
        "fixture_root":root,"host_fallback":false,"limits":limits()})
    );
    if unsupported {
        77
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn immutable_fixture_is_deterministic_and_has_no_eager_transport_reads() {
        let context = HostContext {
            format_version: 1,
            fixture_id: Uuid::new_v4().to_string(),
            root: "/unused".into(),
            entries: 1025,
        };
        let (one, refs) = SyntheticSource::build(&context).unwrap();
        let (two, again) = SyntheticSource::build(&context).unwrap();
        assert_eq!(refs.root, again.root);
        assert_eq!(refs.directory, again.directory);
        assert_eq!(one.receipt(), two.receipt());
        assert_eq!(one.snapshot().fetch_calls, 0);
        let first = PagedDirectory::new(one.as_ref())
            .lookup(&refs.directory, b"file-00000000")
            .unwrap()
            .unwrap();
        assert!(matches!(
            first.node,
            EntryNode::File {
                logical_size: FILE_LOGICAL_BYTES,
                ..
            }
        ));
        assert_eq!(one.snapshot().fetch_calls, 0);
        assert!(one.receipt().total_bytes < 8 * 1024 * 1024);
    }
    #[test]
    fn synthetic_authority_requires_the_entire_current_binding() {
        let context = HostContext {
            format_version: 1,
            fixture_id: Uuid::new_v4().to_string(),
            root: "/unused".into(),
            entries: 1,
        };
        let (_, refs) = SyntheticSource::build(&context).unwrap();
        let request = selection(&context, &refs).unwrap();
        let binding = PartialBinding {
            protocol: crate::vfs::partial_rpc::PROTOCOL.into(),
            format_version: 1,
            launch_request_digest: sha(&canonical_json(&request).unwrap()),
            root: request.root.clone(),
            attempt: request.attempt.clone(),
            observation: request.observation.clone(),
        };
        let authority = SyntheticAuthority {
            binding: binding.clone(),
            revoked: AtomicBool::new(false),
        };
        PartialAttemptAuthority::validate(&authority, &binding).unwrap();
        CurrentAttemptAuthority::validate(&authority, &request.attempt, &request.root).unwrap();
        let mut stale = binding.clone();
        stale.root.scope.tenant_id = Uuid::new_v4().to_string();
        assert!(PartialAttemptAuthority::validate(&authority, &stale).is_err());
        assert!(
            CurrentAttemptAuthority::validate(&authority, &request.attempt, &stale.root).is_err()
        );
        authority.revoked.store(true, Ordering::Release);
        assert!(PartialAttemptAuthority::validate(&authority, &binding).is_err());
        assert!(
            CurrentAttemptAuthority::validate(&authority, &request.attempt, &request.root).is_err()
        );
    }
}
