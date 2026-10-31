//! Explicit partial-root VFS resolver. Inherited references are reached only
//! by immutable root-relative origin paths and opaque RemoteSession proofs.
//! Local CAS presence never grants remote membership. This first profile has
//! empty inherited topology; runtime hardlinks remain ordinary private inodes.
//! Checkpoint/observation is gated until typed partial-delta retention exists.

use super::*;
use crate::{
    content::{self as file, CHUNK_BYTES},
    directory::{self as paged, EntryNode as PagedNode, ShardNode},
    remote_root::{
        AttemptBinding, LoadedObject, ObjectHandle, RemoteSession, RootBinding, RootError,
    },
    snapshot::{self, RunnableRoot},
    workspace::WorkspaceProfile,
};
use std::time::Instant;

#[derive(Debug, Clone)]
pub struct RemoteOptions {
    pub request_timeout: Duration,
    pub max_cached_objects: usize,
    pub max_cached_bytes: u64,
    pub max_listing_entries: usize,
    pub max_listing_pages: usize,
    pub max_origin_bytes: usize,
    pub max_initial_bytes: u64,
    pub max_initial_objects: usize,
    pub readdir: ReaddirLimits,
}
impl Default for RemoteOptions {
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs(30),
            max_cached_objects: 64,
            max_cached_bytes: 8 * 1024 * 1024,
            max_listing_entries: 4096,
            max_listing_pages: 256,
            max_origin_bytes: 16 * 1024,
            max_initial_bytes: 512 * 1024,
            max_initial_objects: 16,
            readdir: ReaddirLimits::default(),
        }
    }
}
impl RemoteOptions {
    fn validate(&self) -> Result<()> {
        self.readdir.validate()?;
        ensure!(
            !self.request_timeout.is_zero()
                && self.request_timeout <= Duration::from_secs(120)
                && self.max_cached_objects > 0
                && self.max_cached_objects <= 1024
                && self.max_cached_bytes >= CHUNK_BYTES as u64
                && self.max_cached_bytes <= 64 * 1024 * 1024
                && self.max_listing_entries > 0
                && self.max_listing_entries <= 65536
                && self.max_listing_pages > 0
                && self.max_listing_pages <= 4096
                && self.max_origin_bytes > 0
                && self.max_origin_bytes <= 64 * 1024
                && self.max_initial_bytes > 0
                && self.max_initial_bytes <= 4 * 1024 * 1024
                && self.max_initial_objects > 0
                && self.max_initial_objects <= 64,
            "invalid remote VFS limits"
        );
        Ok(())
    }
}
#[derive(Debug, Clone, Serialize)]
pub struct RemoteStats {
    pub cached_objects: usize,
    pub cached_encoded_bytes: u64,
    pub requests: u64,
    pub cache_hits: u64,
    pub evictions: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Binding {
    format_version: u32,
    pub(super) root: RootBinding,
    pub(super) attempt: AttemptBinding,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    launch: Option<super::remote_launch::RemoteLaunchRequest>,
}
struct Cache {
    objects: BTreeMap<String, LoadedObject>,
    lru: VecDeque<String>,
    bytes: u64,
    requests: u64,
    hits: u64,
    evictions: u64,
}
pub(super) struct NamePage {
    pub entries: Vec<paged::DirectoryEntry>,
    pub eof: bool,
}
pub(super) struct Resolver {
    session: RemoteSession,
    binding: Binding,
    root: LoadedObject,
    directory: LoadedObject,
    manifest: RunnableRoot,
    profile: WorkspaceProfile,
    header: (u32, u64, u64),
    options: RemoteOptions,
    cache: Mutex<Cache>,
}
fn mapped<T>(value: std::result::Result<T, RootError>) -> Result<T> {
    value.map_err(|e| error(e.errno()))
}
impl Resolver {
    fn new(session: RemoteSession, expected: &ObjectRef, options: RemoteOptions) -> Result<Self> {
        Self::new_inner(session, expected, options, None)
    }
    pub(super) fn for_launch(
        session: RemoteSession,
        expected: &super::remote_launch::RemoteLaunchRequest,
        options: RemoteOptions,
    ) -> Result<Self> {
        Self::new_inner(session, &expected.root.root, options, Some(expected))
    }
    fn new_inner(
        session: RemoteSession,
        expected: &ObjectRef,
        options: RemoteOptions,
        launch: Option<&super::remote_launch::RemoteLaunchRequest>,
    ) -> Result<Self> {
        options.validate()?;
        ensure!(
            session.binding().root == *expected && expected.kind == ObjectKind::RunnableRoot,
            "remote VFS expected RunnableRoot differs"
        );
        let mut bytes = 0u64;
        let mut objects = 0usize;
        let launch_deadline = Instant::now() + options.request_timeout;
        let initial =
            |handle: ObjectHandle, bytes: &mut u64, objects: &mut usize| -> Result<LoadedObject> {
                if handle.reference().size_bytes > 64 * 1024 {
                    return Err(error(libc::EOPNOTSUPP))
                        .context("remote startup metadata requires explicit bulk path");
                }
                *bytes = bytes
                    .checked_add(handle.reference().size_bytes)
                    .context("remote startup bytes overflow")?;
                *objects = objects
                    .checked_add(1)
                    .context("remote startup count overflow")?;
                if *bytes > options.max_initial_bytes || *objects > options.max_initial_objects {
                    return Err(error(libc::EAGAIN));
                }
                let deadline = if launch.is_some() {
                    launch_deadline
                } else {
                    Instant::now() + options.request_timeout
                };
                mapped(mapped(session.request(&handle, deadline))?.wait())
            };
        let root = initial(mapped(session.root())?, &mut bytes, &mut objects)?;
        let manifest = mapped(root.with_bytes(snapshot::decode_runnable))??;
        if let Some(expected) = launch {
            if manifest.environment.is_some() {
                return Err(error(libc::EOPNOTSUPP))
                    .context("remote launch does not support environment composition");
            }
            ensure!(
                manifest.source == expected.accepted_source
                    && manifest.profile == expected.observation.profile,
                "remote launch source/profile differs from the expected selection"
            );
        }
        let source = initial(
            mapped(root.child(&manifest.source))?,
            &mut bytes,
            &mut objects,
        )?;
        let source_manifest = mapped(source.with_bytes(snapshot::decode_source))??;
        if let Some(expected) = launch {
            if source_manifest.directory != manifest.view_root {
                return Err(error(libc::EOPNOTSUPP))
                    .context("remote launch source directory must equal runnable view root");
            }
            ensure!(
                source_manifest.projection_policy == expected.projection_policy,
                "remote launch projection policy differs from the expected selection"
            );
            let policy = initial(
                mapped(source.child(&source_manifest.projection_policy))?,
                &mut bytes,
                &mut objects,
            )?;
            mapped(policy.with_bytes(|bytes| -> Result<()> {
                let value: serde_json::Value = serde_json::from_slice(bytes)?;
                ensure!(
                    value.is_object() && canonical_json(&value)? == bytes,
                    "remote launch policy must be canonical metadata"
                );
                Ok(())
            }))??;
        }
        if let Some(reference) = &manifest.environment {
            let env = initial(mapped(root.child(reference))?, &mut bytes, &mut objects)?;
            mapped(env.with_bytes(snapshot::decode_environment))??;
        }
        let profile = initial(
            mapped(root.child(&manifest.profile))?,
            &mut bytes,
            &mut objects,
        )?;
        let profile: WorkspaceProfile =
            mapped(profile.with_bytes(|bytes| -> Result<WorkspaceProfile> {
                let profile: WorkspaceProfile = serde_json::from_slice(bytes)?;
                ensure!(
                    canonical_json(&profile)? == bytes,
                    "remote profile is not canonical"
                );
                ensure!(
                    profile.prepared.len() + profile.scratch.len() + profile.vcs_local.len() <= 128,
                    "remote profile partition budget"
                );
                profile.validate()?;
                Ok(profile)
            }))??;
        if launch.is_some() && profile != WorkspaceProfile::source_only() {
            return Err(error(libc::EOPNOTSUPP))
                .context("remote launch requires source-only profile without partitions");
        }
        let topology = initial(
            mapped(root.child(&manifest.hardlinks))?,
            &mut bytes,
            &mut objects,
        )?;
        let topology: HardlinkTopology =
            mapped(topology.with_bytes(|bytes| -> Result<HardlinkTopology> {
                let topology: HardlinkTopology = serde_json::from_slice(bytes)?;
                ensure!(
                    canonical_json(&topology)? == bytes && topology.format_version == 1,
                    "invalid remote topology"
                );
                Ok(topology)
            }))??;
        if !topology.groups.is_empty() {
            return Err(error(libc::EOPNOTSUPP))
                .context("inherited hardlink groups require explicit qualified bulk path");
        }
        let directory = initial(
            mapped(root.child(&manifest.view_root))?,
            &mut bytes,
            &mut objects,
        )?;
        let header = mapped(directory.with_bytes(paged::decode_directory_root))??;
        // Actual root shard summaries, not just a claimed count, bound private
        // namespace counters. This does not visit descendant pages or files.
        initial(
            mapped(directory.child(&header.directory_shard))?,
            &mut bytes,
            &mut objects,
        )?;
        if launch.is_some() && Instant::now() >= launch_deadline {
            return Err(error(libc::ETIMEDOUT));
        }
        let binding = Binding {
            format_version: if launch.is_some() { 2 } else { 1 },
            root: session.binding().clone(),
            attempt: session.attempt().clone(),
            launch: launch.cloned(),
        };
        Ok(Self {
            session,
            binding,
            root,
            directory,
            manifest,
            profile,
            header: (header.mode, header.entry_count, header.subdirectory_count),
            options,
            cache: Mutex::new(Cache {
                objects: BTreeMap::new(),
                lru: VecDeque::new(),
                bytes: 0,
                requests: objects as u64,
                hits: 0,
                evictions: 0,
            }),
        })
    }
    pub(super) fn guard(&self) -> Result<()> {
        mapped(self.root.with_bytes(|_| ()))
    }
    pub(super) fn profile(&self) -> &WorkspaceProfile {
        &self.profile
    }
    pub(super) fn launch_request(&self) -> Option<&super::remote_launch::RemoteLaunchRequest> {
        self.binding.launch.as_ref()
    }
    /// Persisted metadata is checked only against this actual current resolver;
    /// parsing a saved binding never creates a session or grants permission.
    pub(super) fn persisted_binding(&self, bytes: &str, namespace: &str) -> Result<Binding> {
        self.guard()?;
        ensure!(bytes.len() <= 64 * 1024, "remote binding exceeds 64 KiB");
        let binding: Binding = serde_json::from_str(bytes)?;
        match (binding.format_version, &binding.launch) {
            (1, None) => {}
            (2, Some(request)) => {
                request.validate()?;
                ensure!(
                    request.root == binding.root
                        && request.attempt == binding.attempt
                        && request.observation.namespace == namespace
                        && request.observation.profile == self.manifest.profile
                        && request.accepted_source == self.manifest.source,
                    "persisted launch binding differs from namespace/root/attempt/profile"
                );
            }
            _ => anyhow::bail!("unknown remote capture binding"),
        }
        ensure!(
            binding == self.binding && bytes == self.serialized_binding()?.as_str(),
            "persisted remote capture binding differs from the actual current resolver"
        );
        Ok(binding)
    }
    pub(super) fn serialized_binding(&self) -> Result<String> {
        Ok(serde_json::to_string(&self.binding)?)
    }
    pub(super) fn view_id(&self) -> &str {
        &self.binding.attempt.view_id
    }
    pub(super) fn owner(&self) -> &OwnerEpoch {
        &self.binding.attempt.owner
    }
    pub(super) fn base(&self) -> &ObjectRef {
        &self.manifest.view_root
    }
    pub(super) fn session(&self) -> &RemoteSession {
        &self.session
    }
    pub(super) fn topology(&self) -> &ObjectRef {
        &self.manifest.hardlinks
    }
    pub(super) fn topology_handle(&self) -> Result<ObjectHandle> {
        mapped(self.root.child(&self.manifest.hardlinks))
    }
    /// Derive membership without reading the final File/Blob payload. Only the
    /// bounded immutable directory route is loaded, including moved origins.
    pub(super) fn capture_handle(
        &self,
        origin: Option<&str>,
        expected: &ObjectRef,
        meter: &observation::WorkMeter<'_>,
    ) -> Result<ObjectHandle> {
        let path = self.origin(origin)?;
        let mut directory = self.directory.clone();
        if path.is_empty() {
            ensure!(expected == self.base(), "remote root origin differs");
            return mapped(self.root.child(expected));
        }
        let mut prefix = String::new();
        for (index, name) in path.iter().enumerate() {
            meter.check()?;
            let (page, entry) = self
                .lookup_in_metered(&directory, &prefix, name, Some(meter))?
                .ok_or_else(|| error(libc::ENOENT))?;
            let object = match entry.node {
                PagedNode::Directory { object } => object,
                PagedNode::File { object, .. } if index + 1 == path.len() => object,
                _ => return Err(error(libc::ELOOP)),
            };
            let handle = mapped(page.child(&object))?;
            if index + 1 == path.len() {
                ensure!(&object == expected, "remote capture origin differs");
                return Ok(handle);
            }
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(&hex::encode(name));
            meter.metadata(object.size_bytes)?;
            directory = self.load(format!("resolved:{prefix}"), handle)?;
        }
        Err(error(libc::EIO))
    }
    pub(super) fn check_origin(&self, origin: &str) -> Result<()> {
        self.origin(Some(origin)).map(|_| ())
    }
    fn origin(&self, origin: Option<&str>) -> Result<Vec<Vec<u8>>> {
        let origin = origin.context("remote inherited object lacks immutable origin")?;
        if origin.len() > self.options.max_origin_bytes {
            return Err(error(libc::ENAMETOOLONG));
        }
        if origin.is_empty() {
            return Ok(Vec::new());
        }
        let parts = origin.split('/').map(str::to_string).collect::<Vec<_>>();
        decode_path(&parts)
    }
    fn load(&self, key: String, handle: ObjectHandle) -> Result<LoadedObject> {
        self.guard()?;
        let cost = handle.reference().size_bytes.max(1);
        {
            let mut cache = self.cache.lock().map_err(|_| error(libc::EIO))?;
            if let Some(value) = cache.objects.get(&key).cloned() {
                ensure!(
                    value.reference() == handle.reference(),
                    "remote proof cache reference differs"
                );
                mapped(value.with_bytes(|_| ()))?;
                cache.hits = cache.hits.saturating_add(1);
                cache.lru.retain(|k| k != &key);
                cache.lru.push_back(key);
                return Ok(value);
            }
            // Release unused snapshots before admitting another immutable
            // payload. Parent proof chains remain accounted by RemoteSession.
            while cache.objects.len() >= self.options.max_cached_objects
                || cache
                    .bytes
                    .checked_add(cost)
                    .is_none_or(|bytes| bytes > self.options.max_cached_bytes)
            {
                let Some(old) = cache.lru.pop_front() else {
                    break;
                };
                if let Some(value) = cache.objects.remove(&old) {
                    cache.bytes -= value.reference().size_bytes.max(1);
                    cache.evictions = cache.evictions.saturating_add(1);
                }
            }
        }
        let data = mapped(
            mapped(
                self.session
                    .request(&handle, Instant::now() + self.options.request_timeout),
            )?
            .wait(),
        )?;
        let cost = data.reference().size_bytes.max(1);
        let mut cache = self.cache.lock().map_err(|_| error(libc::EIO))?;
        cache.requests = cache.requests.saturating_add(1);
        if cost <= self.options.max_cached_bytes {
            cache.lru.retain(|existing| existing != &key);
            if let Some(old) = cache.objects.remove(&key) {
                cache.bytes -= old.reference().size_bytes.max(1);
            }
            while cache.objects.len() >= self.options.max_cached_objects
                || cache
                    .bytes
                    .checked_add(cost)
                    .is_none_or(|bytes| bytes > self.options.max_cached_bytes)
            {
                let Some(old) = cache.lru.pop_front() else {
                    return Err(error(libc::EAGAIN));
                };
                if let Some(value) = cache.objects.remove(&old) {
                    cache.bytes -= value.reference().size_bytes.max(1);
                    cache.evictions = cache.evictions.saturating_add(1);
                }
            }
            if let Some(old) = cache.objects.insert(key.clone(), data.clone()) {
                cache.bytes -= old.reference().size_bytes.max(1)
            }
            cache.bytes = cache
                .bytes
                .checked_add(cost)
                .context("remote cache bytes overflow")?;
            cache.lru.push_back(key);
        }
        Ok(data)
    }
    fn cached_origin(&self, origin: &str, expected: &ObjectRef) -> Result<Option<LoadedObject>> {
        self.guard()?;
        let key = format!("resolved:{origin}");
        let mut cache = self.cache.lock().map_err(|_| error(libc::EIO))?;
        let Some(value) = cache.objects.get(&key).cloned() else {
            return Ok(None);
        };
        ensure!(
            value.reference() == expected,
            "cached origin differs from inherited reference"
        );
        mapped(value.with_bytes(|_| ()))?;
        cache.hits = cache.hits.saturating_add(1);
        cache.lru.retain(|existing| existing != &key);
        cache.lru.push_back(key);
        Ok(Some(value))
    }
    fn edge(&self, parent: &LoadedObject, index: usize, key: &str) -> Result<LoadedObject> {
        let handle = mapped(parent.child_at(index))?;
        self.load(
            format!("{key}:{}:{index}", parent.reference().digest),
            handle,
        )
    }
    fn child(
        &self,
        parent: &LoadedObject,
        reference: &ObjectRef,
        key: &str,
    ) -> Result<LoadedObject> {
        let handle = mapped(parent.child(reference))?;
        self.load(
            format!("{key}:{}:{}", parent.reference().digest, reference.digest),
            handle,
        )
    }
    fn lookup_in(
        &self,
        directory: &LoadedObject,
        origin: &str,
        name: &[u8],
    ) -> Result<Option<(LoadedObject, paged::DirectoryEntry)>> {
        self.lookup_in_metered(directory, origin, name, None)
    }
    fn lookup_in_metered(
        &self,
        directory: &LoadedObject,
        origin: &str,
        name: &[u8],
        meter: Option<&observation::WorkMeter<'_>>,
    ) -> Result<Option<(LoadedObject, paged::DirectoryEntry)>> {
        name_ok(name)?;
        if let Some(meter) = meter {
            meter.metadata(directory.reference().size_bytes)?;
        }
        let header = mapped(directory.with_bytes(paged::decode_directory_root))??;
        if let Some(meter) = meter {
            meter.metadata(header.directory_shard.size_bytes)?;
        }
        let mut current = self.child(
            directory,
            &header.directory_shard,
            &format!("page:{origin}"),
        )?;
        for _ in 0..=255 {
            let shard = mapped(current.with_bytes(paged::decode_shard))??;
            match shard.node {
                ShardNode::Leaf { entries } => {
                    return Ok(entries
                        .into_iter()
                        .find(|entry| entry.name().is_ok_and(|raw| raw == name))
                        .map(|entry| (current, entry)))
                }
                ShardNode::Branch { children } => {
                    let prefix = hex::decode(shard.prefix_hex)?;
                    if !name.starts_with(&prefix) {
                        return Ok(None);
                    }
                    let edge = if name.len() == prefix.len() {
                        0
                    } else {
                        u16::from(name[prefix.len()]) + 1
                    };
                    let Some(index) = children.iter().position(|child| child.edge == edge) else {
                        return Ok(None);
                    };
                    if let Some(meter) = meter {
                        meter.metadata(children[index].shard.size_bytes)?;
                    }
                    current = self.edge(&current, index, &format!("page:{origin}"))?;
                }
            }
        }
        Err(error(libc::ELOOP))
    }
    fn resolve(&self, origin: Option<&str>, expected: &ObjectRef) -> Result<LoadedObject> {
        let path = self.origin(origin)?;
        if let Some(cached) = self.cached_origin(origin.unwrap_or(""), expected)? {
            return Ok(cached);
        }
        let mut directory = self.directory.clone();
        let mut prefix = String::new();
        for (index, name) in path.iter().enumerate() {
            let (page, entry) = self
                .lookup_in(&directory, &prefix, name)?
                .ok_or_else(|| error(libc::ENOENT))?;
            if !prefix.is_empty() {
                prefix.push('/')
            }
            prefix.push_str(&hex::encode(name));
            let object = match entry.node {
                PagedNode::Directory { object } => object,
                PagedNode::File { object, .. } if index + 1 == path.len() => object,
                _ => return Err(error(libc::ELOOP)),
            };
            directory = self.load(format!("resolved:{prefix}"), mapped(page.child(&object))?)?;
        }
        ensure!(
            directory.reference() == expected,
            "remote inherited reference differs from immutable origin"
        );
        mapped(directory.with_bytes(|_| ()))?;
        Ok(directory)
    }
    pub(super) fn header(
        &self,
        origin: Option<&str>,
        expected: &ObjectRef,
    ) -> Result<(u32, u64, u64)> {
        let directory = self.resolve(origin, expected)?;
        let root = mapped(directory.with_bytes(paged::decode_directory_root))??;
        self.child(
            &directory,
            &root.directory_shard,
            &format!("page:{}", origin.unwrap_or("")),
        )?;
        Ok((root.mode, root.entry_count, root.subdirectory_count))
    }
    pub(super) fn lookup(
        &self,
        origin: Option<&str>,
        expected: &ObjectRef,
        name: &[u8],
    ) -> Result<Option<paged::DirectoryEntry>> {
        let directory = self.resolve(origin, expected)?;
        let Some((page, entry)) = self.lookup_in(&directory, origin.unwrap_or(""), name)? else {
            return Ok(None);
        };
        if let PagedNode::File { object, .. } = &entry.node {
            if object.kind == ObjectKind::File {
                let prefix = if origin.unwrap_or("").is_empty() {
                    hex::encode(name)
                } else {
                    format!("{}/{}", origin.unwrap_or(""), hex::encode(name))
                };
                self.load(format!("resolved:{prefix}"), mapped(page.child(object))?)?;
            }
        }
        Ok(Some(entry))
    }
    pub(super) fn page_deadline(&self) -> Instant {
        Instant::now() + self.options.request_timeout
    }
    pub(super) fn readdir_limits(&self) -> &ReaddirLimits {
        &self.options.readdir
    }
    /// Entries are verified directory-page declarations, not child attributes.
    /// In particular this operation NEVER fetches File descriptors or Blobs.
    pub(super) fn scan(
        &self,
        origin: Option<&str>,
        expected: &ObjectRef,
        after: Option<&[u8]>,
        limit: usize,
    ) -> Result<NamePage> {
        ensure!(
            limit > 0 && limit <= readdir::DIRECTORY_PAGE_ENTRIES,
            "remote scan page bound"
        );
        let directory = self.resolve(origin, expected)?;
        let root = mapped(directory.with_bytes(paged::decode_directory_root))??;
        let current = self.child(
            &directory,
            &root.directory_shard,
            &format!("page:{}", origin.unwrap_or("")),
        )?;
        let mut entries = Vec::with_capacity(limit + 1);
        let mut pages = 0;
        let deadline = self.page_deadline();
        self.scan_loaded(
            current,
            origin.unwrap_or(""),
            after,
            limit + 1,
            &mut entries,
            &mut pages,
            0,
            deadline,
        )?;
        let eof = entries.len() <= limit;
        entries.truncate(limit);
        Ok(NamePage { entries, eof })
    }
    fn scan_loaded(
        &self,
        current: LoadedObject,
        origin: &str,
        after: Option<&[u8]>,
        limit: usize,
        output: &mut Vec<paged::DirectoryEntry>,
        pages: &mut usize,
        depth: usize,
        deadline: Instant,
    ) -> Result<()> {
        if Instant::now() >= deadline {
            return Err(error(libc::ETIMEDOUT));
        }
        if depth > 255 {
            return Err(error(libc::ELOOP));
        }
        *pages += 1;
        if *pages > self.options.max_listing_pages {
            return Err(error(libc::EAGAIN));
        }
        let shard = mapped(current.with_bytes(paged::decode_shard))??;
        let prefix = hex::decode(&shard.prefix_hex)?;
        let before = |prefix: &[u8], after: &[u8]| prefix < after && !after.starts_with(prefix);
        if after.is_some_and(|after| before(&prefix, after)) {
            return Ok(());
        }
        match shard.node {
            ShardNode::Leaf { entries } => {
                for entry in entries {
                    if after
                        .is_none_or(|after| entry.name_hex.as_str() > hex::encode(after).as_str())
                    {
                        output.push(entry);
                        if output.len() == limit {
                            break;
                        }
                    }
                }
            }
            ShardNode::Branch { children } => {
                for (index, child) in children.iter().enumerate() {
                    if output.len() == limit {
                        break;
                    }
                    let mut route = prefix.clone();
                    if child.edge == 0 {
                        if after.is_some_and(|after| route.as_slice() <= after) {
                            continue;
                        }
                    } else {
                        route.push((child.edge - 1) as u8);
                        if after.is_some_and(|after| before(&route, after)) {
                            continue;
                        }
                    }
                    let loaded = self.edge(&current, index, &format!("page:{origin}"))?;
                    self.scan_loaded(
                        loaded,
                        origin,
                        after,
                        limit,
                        output,
                        pages,
                        depth + 1,
                        deadline,
                    )?;
                }
            }
        }
        Ok(())
    }
    pub(super) fn names(&self, origin: Option<&str>, expected: &ObjectRef) -> Result<Vec<Vec<u8>>> {
        let directory = self.resolve(origin, expected)?;
        let root = mapped(directory.with_bytes(paged::decode_directory_root))??;
        if root.entry_count > self.options.max_listing_entries as u64 {
            return Err(error(libc::EOVERFLOW));
        }
        let mut pending = vec![self.child(
            &directory,
            &root.directory_shard,
            &format!("page:{}", origin.unwrap_or("")),
        )?];
        let mut pages = 0usize;
        let mut names = Vec::new();
        while let Some(current) = pending.pop() {
            pages += 1;
            if pages > self.options.max_listing_pages {
                return Err(error(libc::EAGAIN));
            }
            let shard = mapped(current.with_bytes(paged::decode_shard))??;
            match shard.node {
                ShardNode::Leaf { entries } => {
                    for entry in entries {
                        names.push(entry.name()?);
                        if names.len() > self.options.max_listing_entries {
                            return Err(error(libc::EOVERFLOW));
                        }
                    }
                }
                ShardNode::Branch { children } => {
                    for index in (0..children.len()).rev() {
                        pending.push(self.edge(
                            &current,
                            index,
                            &format!("page:{}", origin.unwrap_or("")),
                        )?);
                    }
                }
            }
        }
        names.sort();
        ensure!(
            names.len() as u64 == root.entry_count,
            "remote directory count differs after listing"
        );
        Ok(names)
    }
    pub(super) fn read_range(
        &self,
        origin: Option<&str>,
        expected: &ObjectRef,
        offset: u64,
        length: usize,
    ) -> Result<Vec<u8>> {
        ensure!(length <= MAX_IO, "remote range exceeds bound");
        let root = self.resolve(origin, expected)?;
        let size = if expected.kind == ObjectKind::Blob {
            expected.size_bytes
        } else {
            mapped(root.with_bytes(file::decode_file_node))??.logical_size
        };
        let count = size.saturating_sub(offset).min(length as u64) as usize;
        let mut output = Vec::with_capacity(count);
        let end = offset
            .checked_add(count as u64)
            .context("remote range overflow")?;
        let mut position = offset;
        while position < end {
            let mut current = root.clone();
            let mut start = 0u64;
            while current.reference().kind == ObjectKind::File {
                let node = mapped(current.with_bytes(file::decode_file_node))??;
                let mut child_start = start;
                let mut selected = None;
                for index in 0..node.children.len() {
                    let span = node.child_span(index)?;
                    if position < child_start + span {
                        selected = Some((index, child_start));
                        break;
                    }
                    child_start += span;
                }
                let (index, next_start) = selected.context("remote file range exceeds tree")?;
                current = self.edge(
                    &current,
                    index,
                    &format!("content:{}", origin.unwrap_or("")),
                )?;
                start = next_start;
            }
            ensure!(
                current.reference().kind == ObjectKind::Blob,
                "remote file child is not content"
            );
            let within = usize::try_from(position - start)?;
            let take =
                ((end - position) as usize).min(current.reference().size_bytes as usize - within);
            ensure!(take > 0, "remote range made no progress");
            mapped(
                current.with_bytes(|bytes| output.extend_from_slice(&bytes[within..within + take])),
            )?;
            position += take as u64;
        }
        Ok(output)
    }
}
impl VfsManager {
    pub(super) fn open_remote_selection(
        root: impl AsRef<Path>,
        store: LocalStore,
        resolver: Arc<Resolver>,
    ) -> Result<Self> {
        resolver.guard()?;
        ensure!(
            resolver.launch_request().is_some(),
            "remote selection lacks validated launch binding"
        );
        let mut manager = Self::open(root, store)?;
        Arc::get_mut(&mut manager.0)
            .context("remote manager unexpectedly shared")?
            .remote = Some(resolver);
        Ok(manager)
    }
    /// Explicit partial-root profile; inherited CAS is never imported/pinned.
    /// The supplied LocalStore is private local-delta authority only. The
    /// session is fresh/current and binds this exact view UUID and owner.
    pub fn open_remote(
        root: impl AsRef<Path>,
        store: LocalStore,
        session: RemoteSession,
        expected_runnable: &ObjectRef,
        options: RemoteOptions,
    ) -> Result<Self> {
        let resolver = Arc::new(Resolver::new(session, expected_runnable, options)?);
        let mut manager = Self::open(root, store)?;
        Arc::get_mut(&mut manager.0)
            .context("remote manager unexpectedly shared")?
            .remote = Some(resolver);
        Ok(manager)
    }
    pub fn fork_remote(&self) -> Result<VfsView> {
        self.fork_remote_inner(None)
    }
    /// Reserved namespaces are available only on a validated launch selection.
    /// The exact namespace is included in immutable fork request bytes and the
    /// v2 remote binding; neither retry nor reopen can mint a replacement.
    pub fn fork_remote_with_namespace(&self, namespace: &str) -> Result<VfsView> {
        self.fork_remote_inner(Some(namespace))
    }
    fn fork_remote_inner(&self, namespace: Option<&str>) -> Result<VfsView> {
        let remote = self
            .0
            .remote
            .as_ref()
            .context("manager has no remote root")?;
        remote.guard()?;
        match (remote.launch_request(), namespace) {
            (None, None) => {}
            (Some(request), Some(namespace)) => ensure!(
                namespace == request.observation.namespace,
                "remote fork differs from reserved observation namespace"
            ),
            _ => {
                return Err(error(libc::EOPNOTSUPP))
                    .context("remote reserved fork requires its validated selection")
            }
        }
        let (mode, entries, subdirectories) = remote.header;
        ensure!(
            entries <= i64::MAX as u64
                && subdirectories
                    .checked_add(2)
                    .is_some_and(|n| n <= i64::MAX as u64),
            "remote counts exceed namespace capacity"
        );
        self.0.index.lock().map_err(|_| error(libc::EIO))?.execute(
            "INSERT OR REPLACE INTO prepared_paged VALUES(?,?,?,?,?)",
            params![
                remote.base().digest,
                json_ref(remote.base())?,
                mode,
                sql_id(entries),
                sql_id(subdirectories)
            ],
        )?;
        self.fork_inner_with_namespace(
            remote.view_id(),
            remote.base(),
            None,
            remote.owner().clone(),
            true,
            namespace,
        )
    }
    pub fn open_remote_view(&self) -> Result<VfsView> {
        let remote = self
            .0
            .remote
            .as_ref()
            .context("manager has no remote root")?;
        self.open_view(remote.view_id(), remote.owner())
    }
    pub fn remote_root_binding(&self) -> Option<&RootBinding> {
        self.0.remote.as_ref().map(|r| &r.binding.root)
    }
    pub fn remote_profile(&self) -> Option<&WorkspaceProfile> {
        self.0.remote.as_ref().map(|r| &r.profile)
    }
    pub fn remote_stats(&self) -> Result<Option<RemoteStats>> {
        let Some(remote) = &self.0.remote else {
            return Ok(None);
        };
        let cache = remote.cache.lock().map_err(|_| error(libc::EIO))?;
        Ok(Some(RemoteStats {
            cached_objects: cache.objects.len(),
            cached_encoded_bytes: cache.bytes,
            requests: cache.requests,
            cache_hits: cache.hits,
            evictions: cache.evictions,
        }))
    }
}
