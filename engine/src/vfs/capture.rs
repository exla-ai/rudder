//! Owner-bound partial source capture. A retained object set is local data,
//! never a complete pin, writer barrier, accepted source or published root.
//! Candidate bytes and provenance are journaled before retention, and a nonce
//! never recaptures a different generation after that durable boundary.
use super::observation::{normalized_policy, WorkMeter};
use super::*;
use crate::content::{ContentStore, FileNode};
use crate::delta::{self, DeltaBudget, PartialTreeReceipt};
use crate::remote_root::{
    AttemptBinding, LoadedObject, ObjectHandle, RemoteObjectPath, RemoteSession, RootBinding,
};
use crate::store::{metadata_references, ObjectSetReceipt};
use std::cell::RefCell;

const MAX_RECEIPTS: u64 = 128;
const MAX_LIVE: u64 = 16;
const MAX_WITNESS_BYTES: usize = 4 * 1024 * 1024;
const MAX_JOURNAL_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartialCaptureRequest {
    pub nonce: String,
    pub expected_begin: DirtyCursor,
    /// Compare-and-set the data-only capture head, not the mutable base root.
    pub expected_head: Option<String>,
}
#[derive(Debug, Clone, Default)]
pub struct PartialCaptureBudget {
    /// Producer, private-byte verification and proof replay work. Validation
    /// inside retain_partial_tree has its own metadata/object limits below.
    pub work: ObservationBudget,
    pub delta: DeltaBudget,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartialCaptureReceipt {
    pub format_version: u32,
    pub view_id: String,
    pub owner: OwnerEpoch,
    pub attempt: AttemptBinding,
    pub base: RootBinding,
    pub begin: DirtyCursor,
    pub end: DirtyCursor,
    pub exclusion_policy_digest: String,
    pub request_nonce: String,
    pub previous_head: Option<String>,
    pub head_sequence: u64,
    pub retention: PartialTreeReceipt,
    pub consistency: ObservationConsistency,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartialCaptureStatus {
    /// pending/candidate can have an uncertain object-set ACK. Only committed
    /// has a durable head result; retained means data without head adoption.
    pub state: String,
    pub retention: Option<PartialTreeReceipt>,
    pub receipt: Option<PartialCaptureReceipt>,
    pub current: bool,
}
#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Binding {
    request: PartialCaptureRequest,
    policy: String,
    base: RootBinding,
    attempt: AttemptBinding,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Candidate {
    directory: ObjectRef,
    hardlinks: ObjectRef,
    produced: Vec<ObjectRef>,
    boundaries: Vec<RemoteObjectPath>,
    end: DirtyCursor,
    sequence: u64,
}

/// Every new reference is recorded at its producing put. Remote provenance is
/// learned only from opaque, same-session verified parent edges. Local CAS
/// presence is never a fallback for an absent producer or boundary witness.
pub(super) struct Collector<'a> {
    view: &'a VfsView,
    stage: &'a str,
    meter: &'a WorkMeter<'a>,
    limits: &'a DeltaBudget,
    produced: RefCell<BTreeSet<ObjectRef>>,
    boundaries: RefCell<BTreeMap<ObjectRef, ObjectHandle>>,
    path_bytes: std::cell::Cell<usize>,
}
impl<'a> Collector<'a> {
    fn new(
        view: &'a VfsView,
        stage: &'a str,
        meter: &'a WorkMeter<'a>,
        limits: &'a DeltaBudget,
    ) -> Result<Self> {
        let this = Self {
            view,
            stage,
            meter,
            limits,
            produced: RefCell::new(BTreeSet::new()),
            boundaries: RefCell::new(BTreeMap::new()),
            path_bytes: std::cell::Cell::new(0),
        };
        this.insert(
            this.remote()
                .capture_handle(Some(""), this.remote().base(), meter)?,
        )?;
        this.insert(this.remote().topology_handle()?)?;
        Ok(this)
    }
    fn remote(&self) -> &remote::Resolver {
        self.view
            .manager()
            .0
            .remote
            .as_deref()
            .expect("partial collector")
    }
    fn insert(&self, handle: ObjectHandle) -> Result<()> {
        self.meter.check()?;
        self.remote().session().validate_handle(&handle)?;
        let reference = handle.reference().clone();
        if self.boundaries.borrow().contains_key(&reference) {
            return Ok(());
        }
        ensure!(
            self.boundaries.borrow().len() < self.limits.max_boundary_objects,
            "capture remote-boundary quota exhausted"
        );
        let cost = canonical_json(&handle.reference_path()?)?.len();
        let bytes = self
            .path_bytes
            .get()
            .checked_add(cost)
            .context("capture path bytes overflow")?;
        ensure!(
            bytes <= MAX_WITNESS_BYTES,
            "capture boundary-path bytes exhausted"
        );
        self.path_bytes.set(bytes);
        self.boundaries.borrow_mut().insert(reference, handle);
        Ok(())
    }
    pub(super) fn bind(&self, node: &Node) -> Result<()> {
        if let Some(base) = &node.base {
            self.insert(
                self.remote()
                    .capture_handle(node.origin.as_deref(), base, self.meter)?,
            )?;
        }
        Ok(())
    }
    pub(super) fn read_metadata(&self, reference: &ObjectRef) -> Result<Vec<u8>> {
        ensure!(
            matches!(
                reference.kind,
                ObjectKind::Directory | ObjectKind::DirectoryShard | ObjectKind::File
            ),
            "capture metadata read cannot hydrate Blob payloads"
        );
        ensure!(
            reference.size_bytes <= 64 * 1024,
            "capture metadata needs explicit bulk path"
        );
        self.meter.metadata(reference.size_bytes)?;
        if self.produced.borrow().contains(reference) {
            return self.view.manager().store().read_object(reference);
        }
        let handle = self
            .boundaries
            .borrow()
            .get(reference)
            .cloned()
            .context("capture reference lacks producer or authorized boundary")?;
        let mut ticket = self
            .remote()
            .session()
            .request(&handle, self.meter.deadline())?;
        let loaded = loop {
            self.meter.check()?;
            if let Some(result) = ticket.try_result() {
                break result?;
            }
            std::thread::sleep(Duration::from_millis(1));
        };
        let bytes = loaded.with_bytes(|bytes| bytes.to_vec())?;
        reference.verify(&bytes)?;
        let children: BTreeSet<_> = metadata_references(reference.kind, &bytes)?
            .into_iter()
            .collect();
        for child in children {
            if !self.produced.borrow().contains(&child)
                && !self.boundaries.borrow().contains_key(&child)
            {
                self.insert(loaded.child(&child)?)?;
            }
        }
        Ok(bytes)
    }
    pub(super) fn put(&self, kind: ObjectKind, bytes: &[u8]) -> Result<ObjectRef> {
        self.meter.check()?;
        let reference = ObjectRef::from_bytes(kind, bytes);
        if !self.produced.borrow().contains(&reference) {
            ensure!(
                self.produced.borrow().len() < self.limits.max_objects,
                "capture produced-object quota exhausted"
            );
        }
        self.view
            .manager()
            .store()
            .receive_staged(self.stage, &reference, bytes)?;
        self.produced.borrow_mut().insert(reference.clone());
        Ok(reference)
    }
    fn candidate(
        &self,
        directory: ObjectRef,
        end: DirtyCursor,
        sequence: u64,
    ) -> Result<Candidate> {
        Ok(Candidate {
            directory,
            hardlinks: self.remote().topology().clone(),
            produced: self.produced.borrow().iter().cloned().collect(),
            boundaries: self
                .boundaries
                .borrow()
                .values()
                .map(ObjectHandle::reference_path)
                .collect::<std::result::Result<_, _>>()?,
            end,
            sequence,
        })
    }
}
impl ContentStore for Collector<'_> {
    fn put_blob(&self, bytes: &[u8]) -> Result<ObjectRef> {
        self.put(ObjectKind::Blob, bytes)
    }
    fn put_file_node(&self, node: &FileNode) -> Result<ObjectRef> {
        node.validate()?;
        self.put(ObjectKind::File, &canonical_json(node)?)
    }
    fn read_object(&self, reference: &ObjectRef) -> Result<Vec<u8>> {
        self.read_metadata(reference)
    }
}
impl crate::directory::DirectoryStore for Collector<'_> {
    fn read(&self, reference: &ObjectRef) -> Result<Vec<u8>> {
        self.read_metadata(reference)
    }
    fn put(&self, kind: ObjectKind, bytes: &[u8]) -> Result<ObjectRef> {
        self.meter.metadata(bytes.len() as u64)?;
        Collector::put(self, kind, bytes)
    }
}

pub(super) fn initialize(c: &Connection) -> Result<()> {
    c.execute_batch("CREATE TABLE partial_captures(nonce TEXT PRIMARY KEY,binding TEXT NOT NULL,stage TEXT NOT NULL,pin TEXT NOT NULL,state TEXT NOT NULL,candidate TEXT,retention TEXT,result TEXT,release_receipt TEXT);CREATE TABLE partial_capture_head(singleton INTEGER PRIMARY KEY CHECK(singleton=1),nonce TEXT NOT NULL,sequence INTEGER NOT NULL CHECK(sequence>0));")?;
    Ok(())
}
pub(super) fn validate_schema(c: &Connection, expected: Option<&remote::Resolver>) -> Result<()> {
    let count: u64 = c.query_row("SELECT count(*) FROM sqlite_master WHERE type='table' AND name IN ('partial_captures','partial_capture_head')", [], |r| read_id(r,0))?;
    ensure!(
        count == 2,
        "capture namespace4 is missing its journal; recovery required"
    );
    let invalid: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM partial_captures WHERE state NOT IN ('pending','candidate','retained','committed','unavailable','released') OR (state IN ('candidate','retained','committed') AND candidate IS NULL) OR (state IN ('retained','committed') AND retention IS NULL) OR (state='committed' AND result IS NULL))", [], |r| r.get(0))?;
    ensure!(!invalid, "capture journal has an unknown/incomplete record");
    let count: u64 = c.query_row("SELECT count(*) FROM partial_captures", [], |r| {
        read_id(r, 0)
    })?;
    ensure!(
        count <= MAX_RECEIPTS,
        "capture journal record quota exceeded"
    );
    journal_room(c, 0)?;
    let namespace: String = c.query_row(
        "SELECT namespace FROM observation_state WHERE singleton=1",
        [],
        |r| r.get(0),
    )?;
    let remote: String = c.query_row(
        "SELECT value FROM meta WHERE key='remote_binding'",
        [],
        |r| r.get(0),
    )?;
    let remote = expected
        .ok_or_else(|| error(libc::EOPNOTSUPP))?
        .persisted_binding(&remote, &namespace)?;
    let mut q = c.prepare("SELECT nonce,binding,stage,pin,candidate,retention,result,release_receipt FROM partial_captures")?;
    let mut rows = q.query([])?;
    while let Some(row) = rows.next()? {
        let nonce: String = row.get(0)?;
        canonical_nonce(&nonce)?;
        let body: String = row.get(1)?;
        ensure!(
            body.len() <= MAX_WITNESS_BYTES,
            "capture binding exceeds bound"
        );
        let binding: Binding = serde_json::from_str(&body)?;
        ensure!(
            canonical_json(&binding)? == body.as_bytes()
                && binding.request.nonce == nonce
                && binding.base == remote.root
                && binding.attempt == remote.attempt
                && binding.request.expected_begin.namespace == namespace
                && binding.request.expected_begin.view_id == remote.attempt.view_id
                && binding.request.expected_begin.owner == remote.attempt.owner,
            "capture journal binding differs from its owning namespace/attempt"
        );
        canonical_nonce(&row.get::<_, String>(2)?)?;
        ensure!(
            row.get::<_, String>(3)? == format!("vfs:partial:{}:{nonce}", remote.attempt.view_id),
            "capture journal pin differs"
        );
        if let Some(body) = row.get::<_, Option<String>>(4)? {
            ensure!(
                body.len() <= MAX_WITNESS_BYTES,
                "capture candidate exceeds bound"
            );
            let candidate: Candidate = serde_json::from_str(&body)?;
            ensure!(
                canonical_json(&candidate)? == body.as_bytes()
                    && candidate.end == binding.request.expected_begin
                    && candidate.sequence > 0
                    && candidate.directory.kind == ObjectKind::Directory
                    && candidate.hardlinks.kind == ObjectKind::Blob
                    && candidate.produced.len() <= 16_000
                    && candidate.boundaries.len() <= 4096,
                "capture journal candidate differs from its reservation"
            );
            candidate.directory.validate()?;
            candidate.hardlinks.validate()?;
            for reference in &candidate.produced {
                reference.validate()?;
            }
            for path in &candidate.boundaries {
                path.reference.validate()?;
                ensure!(
                    path.edges.len() <= MAX_DEPTH,
                    "capture proof path exceeds bound"
                );
            }
        }
        if let Some(body) = row.get::<_, Option<String>>(5)? {
            let retention: PartialTreeReceipt = serde_json::from_str(&body)?;
            ensure!(
                retention.format_version == 1
                    && retention.local.pin_id == row.get::<_, String>(3)?
                    && retention.local.staging_id == row.get::<_, String>(2)?,
                "capture retention identity differs"
            );
        }
        if let Some(body) = row.get::<_, Option<String>>(6)? {
            let receipt: PartialCaptureReceipt = serde_json::from_str(&body)?;
            ensure!(
                receipt.format_version == 1
                    && receipt.request_nonce == nonce
                    && receipt.begin == binding.request.expected_begin
                    && receipt.end == receipt.begin
                    && receipt.base == binding.base
                    && receipt.attempt == binding.attempt
                    && receipt.exclusion_policy_digest == binding.policy
                    && receipt.previous_head == binding.request.expected_head,
                "capture journal result differs from its reservation"
            );
        }
        if let Some(body) = row.get::<_, Option<String>>(7)? {
            let receipt: ObjectSetReceipt = serde_json::from_str(&body)?;
            ensure!(
                receipt.pin_id == row.get::<_, String>(3)?
                    && receipt.staging_id == row.get::<_, String>(2)?,
                "capture retirement identity differs"
            );
        }
    }
    let version: String = c.query_row(
        "SELECT value FROM meta WHERE key='format_version'",
        [],
        |r| r.get(0),
    )?;
    if version == "5" {
        admission::validate_schema(
            c,
            expected.context("namespace5 requires strict launch binding")?,
        )?;
    }
    head(c)?;
    Ok(())
}
fn existing_schema(c: &Connection, expected: Option<&remote::Resolver>) -> Result<bool> {
    let version: String = c.query_row(
        "SELECT value FROM meta WHERE key='format_version'",
        [],
        |r| r.get(0),
    )?;
    match version.as_str() {
        "4" | "5" => {
            validate_schema(c, expected)?;
            Ok(true)
        }
        "3" => {
            let count: u64 = c.query_row("SELECT count(*) FROM sqlite_master WHERE name IN ('partial_captures','partial_capture_head')", [], |r| read_id(r,0))?;
            ensure!(
                count == 0,
                "capture journal exists without activated schema; recovery required"
            );
            Ok(false)
        }
        _ => Err(error(libc::EOPNOTSUPP)).context("partial capture requires remote namespace3/4/5"),
    }
}
fn activate(c: &Connection, expected: Option<&remote::Resolver>) -> Result<()> {
    if existing_schema(c, expected)? {
        return Ok(());
    }
    let tx = c.unchecked_transaction()?;
    initialize(&tx)?;
    tx.execute("UPDATE meta SET value='4' WHERE key='format_version'", [])?;
    tx.commit()?;
    Ok(())
}
pub(super) fn head(c: &Connection) -> Result<Option<(String, u64)>> {
    let head: Option<(String, u64)> = c
        .query_row(
            "SELECT nonce,sequence FROM partial_capture_head WHERE singleton=1",
            [],
            |r| Ok((r.get(0)?, read_id(r, 1)?)),
        )
        .optional()?;
    if let Some((nonce, sequence)) = &head {
        canonical_nonce(nonce)?;
        let result: String = c
            .query_row(
                "SELECT result FROM partial_captures WHERE nonce=? AND state='committed'",
                [nonce],
                |r| r.get(0),
            )
            .context("capture head has no committed record; recovery required")?;
        let receipt: PartialCaptureReceipt = serde_json::from_str(&result)?;
        ensure!(
            receipt.request_nonce == *nonce && receipt.head_sequence == *sequence,
            "capture head sequence differs from its receipt"
        );
    } else {
        ensure!(
            !c.query_row(
                "SELECT EXISTS(SELECT 1 FROM partial_captures WHERE state='committed')",
                [],
                |r| r.get::<_, bool>(0)
            )?,
            "capture head disappeared; recovery required"
        );
    }
    Ok(head)
}
fn journal_room(c: &Connection, additional: usize) -> Result<()> {
    let bytes: u64 = c.query_row("SELECT coalesce(sum(length(CAST(binding AS BLOB))+coalesce(length(CAST(candidate AS BLOB)),0)+coalesce(length(CAST(retention AS BLOB)),0)+coalesce(length(CAST(result AS BLOB)),0)+coalesce(length(CAST(release_receipt AS BLOB)),0)),0) FROM partial_captures", [], |r| read_id(r, 0))?;
    ensure!(
        bytes
            .checked_add(additional as u64)
            .is_some_and(|b| b <= MAX_JOURNAL_BYTES),
        "partial capture journal byte quota exhausted"
    );
    Ok(())
}
fn canonical_nonce(nonce: &str) -> Result<()> {
    ensure!(
        Uuid::parse_str(nonce).is_ok_and(|u| u.to_string() == nonce),
        "capture nonce must be a canonical UUID"
    );
    Ok(())
}

/// Replay only saved metadata paths, sharing their verified common prefixes.
/// Calling resolve_path independently for every leaf would retain many copies
/// of the same ancestor loans and could exhaust a partial session unnecessarily.
fn replay_boundaries(
    session: &RemoteSession,
    paths: &[RemoteObjectPath],
    meter: &WorkMeter<'_>,
) -> Result<Vec<ObjectHandle>> {
    let mut handles = BTreeMap::from([(Vec::<u32>::new(), session.root()?)]);
    let mut loaded: BTreeMap<Vec<u32>, LoadedObject> = BTreeMap::new();
    let mut key_bytes = 0usize;
    let mut result = Vec::with_capacity(paths.len());
    for path in paths {
        meter.check()?;
        let mut prefix = Vec::new();
        for edge in &path.edges {
            if !loaded.contains_key(&prefix) {
                let handle = &handles[&prefix];
                ensure!(
                    handle.reference().kind != ObjectKind::Blob
                        && handle.reference().size_bytes <= 64 * 1024,
                    "capture proof replay cannot hydrate payloads or bulk metadata"
                );
                meter.metadata(handle.reference().size_bytes)?;
                let mut ticket = session.request(handle, meter.deadline())?;
                let parent = loop {
                    meter.check()?;
                    if let Some(value) = ticket.try_result() {
                        break value?;
                    }
                    std::thread::sleep(Duration::from_millis(1));
                };
                loaded.insert(prefix.clone(), parent);
            }
            let mut next = prefix.clone();
            next.push(*edge);
            if !handles.contains_key(&next) {
                key_bytes = key_bytes
                    .checked_add(next.len() * 4)
                    .context("capture replay path-byte overflow")?;
                ensure!(
                    key_bytes <= MAX_WITNESS_BYTES && handles.len() < 20_000,
                    "capture replay prefix quota exhausted"
                );
                handles.insert(next.clone(), loaded[&prefix].child_at(*edge as usize)?);
            }
            prefix = next;
        }
        let handle = &handles[&prefix];
        session.validate_handle(handle)?;
        ensure!(
            handle.reference() == &path.reference,
            "capture boundary replay differs from final reference"
        );
        result.push(handle.clone());
    }
    Ok(result)
}

enum CaptureAdmission<'a> {
    Live(&'a admission::LiveEffect),
    Frozen(&'a FrozenRemoteView),
}
impl CaptureAdmission<'_> {
    fn validate(
        &self,
        view: &VfsView,
        c: &Connection,
        request: &PartialCaptureRequest,
    ) -> Result<()> {
        match self {
            Self::Live(effect) => effect.validate(view),
            Self::Frozen(frozen) => frozen.validate(view, c, request),
        }
    }
}
impl VfsView {
    pub fn capture_partial_source(
        &self,
        policy: &ImportPolicy,
        request: &PartialCaptureRequest,
        budget: &PartialCaptureBudget,
    ) -> Result<PartialCaptureReceipt> {
        self.capture_partial_source_cancellable(policy, request, budget, &|| false)
    }
    /// Owner-only cooperative live capture. Holds the logical namespace lock;
    /// matching begin/end generations do not settle mmap, FDs or writers.
    pub fn capture_partial_source_cancellable(
        &self,
        policy: &ImportPolicy,
        request: &PartialCaptureRequest,
        budget: &PartialCaptureBudget,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<PartialCaptureReceipt> {
        let effect = self.admit_live_effect()?;
        self.capture_partial_source_admitted(
            policy,
            request,
            budget,
            cancelled,
            CaptureAdmission::Live(&effect),
        )
    }
    pub(super) fn capture_partial_source_frozen(
        &self,
        frozen: &FrozenRemoteView,
        budget: &PartialCaptureBudget,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<PartialCaptureReceipt> {
        self.capture_partial_source_admitted(
            &ImportPolicy::default(),
            &frozen.intent().request.capture,
            budget,
            cancelled,
            CaptureAdmission::Frozen(frozen),
        )
    }
    fn capture_partial_source_admitted(
        &self,
        policy: &ImportPolicy,
        request: &PartialCaptureRequest,
        budget: &PartialCaptureBudget,
        cancelled: &dyn Fn() -> bool,
        admission: CaptureAdmission<'_>,
    ) -> Result<PartialCaptureReceipt> {
        let remote = self
            .manager()
            .0
            .remote
            .as_ref()
            .context("partial capture requires a remote view")?;
        canonical_nonce(&request.nonce)?;
        if let Some(nonce) = &request.expected_head {
            canonical_nonce(nonce)?;
        }
        ensure!(
            budget.work.max_elapsed_ms > 0
                && budget.work.max_elapsed_ms <= 120_000
                && budget.work.max_changed_entries <= 100_000
                && budget.work.max_content_bytes <= 256 * 1024 * 1024
                && budget.work.max_metadata_bytes <= 128 * 1024 * 1024
                && budget.delta.max_objects > 0
                && budget.delta.max_objects <= 16_000
                && budget.delta.max_boundary_objects > 0
                && budget.delta.max_boundary_objects <= 4096
                && budget.delta.max_metadata_bytes > 0
                && budget.delta.max_metadata_bytes <= 64 * 1024 * 1024
                && !budget.delta.timeout.is_zero()
                && budget.delta.timeout <= Duration::from_secs(120),
            "invalid partial capture budget"
        );
        let (policy, digest) = normalized_policy(policy)?;
        let meter = WorkMeter::cancellable(&budget.work, cancelled)?;
        let mut c = self.db()?;
        admission.validate(self, &c, request)?;
        match &admission {
            CaptureAdmission::Live(_) => mutable(&c)?,
            CaptureAdmission::Frozen(_) => {
                let terminal: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM meta WHERE key IN ('sealed','destroyed') AND value='true')", [], |r| r.get(0))?;
                ensure!(!terminal, "terminal view cannot capture");
            }
        }
        if !existing_schema(&c, self.manager().0.remote.as_deref())? {
            ensure!(
                self.observation_cursor(&c)? == request.expected_begin,
                "capture begin cursor differs"
            );
            ensure!(
                request.expected_head.is_none(),
                "unactivated capture namespace has no head"
            );
        }
        activate(&c, self.manager().0.remote.as_deref())?;
        let binding = Binding {
            request: request.clone(),
            policy: digest,
            base: remote.session().binding().clone(),
            attempt: remote.session().attempt().clone(),
        };
        let encoded = String::from_utf8(canonical_json(&binding)?)?;
        let existing: Option<(String, String, String, String, Option<String>, Option<String>)> = c.query_row(
            "SELECT binding,stage,pin,state,candidate,result FROM partial_captures WHERE nonce=?", [&request.nonce],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))).optional()?;
        let pin = format!("vfs:partial:{}:{}", self.id(), request.nonce);
        let (stage, previous_candidate) = if let Some((
            stored,
            stage,
            stored_pin,
            state,
            candidate,
            result,
        )) = existing
        {
            ensure!(
                stored == encoded && stored_pin == pin,
                "capture nonce bound to another request/attempt/policy"
            );
            ensure!(state != "released", "capture nonce has been retired");
            ensure!(
                state != "unavailable",
                "capture candidate unavailable; use a new nonce"
            );
            if let Some(result) = result {
                let receipt: PartialCaptureReceipt = serde_json::from_str(&result)?;
                self.validate_partial_receipt(&c, &binding, &receipt)?;
                meter.check()?;
                remote.guard()?;
                admission.validate(self, &c, request)?;
                return Ok(receipt);
            }
            (stage, candidate)
        } else {
            ensure!(
                self.observation_cursor(&c)? == request.expected_begin,
                "capture begin cursor differs"
            );
            ensure!(
                head(&c)?.map(|h| h.0) == request.expected_head,
                "partial capture head changed"
            );
            let (total, live): (u64, u64) = c.query_row(
                "SELECT count(*),coalesce(sum(state!='released'),0) FROM partial_captures",
                [],
                |r| Ok((read_id(r, 0)?, read_id(r, 1)?)),
            )?;
            ensure!(
                total < MAX_RECEIPTS && live < MAX_LIVE,
                "partial capture receipt quota exhausted"
            );
            journal_room(&c, encoded.len())?;
            let stage = self
                .manager()
                .store()
                .begin_staging(Duration::from_secs(3600))?
                .id;
            c.execute("INSERT INTO partial_captures(nonce,binding,stage,pin,state) VALUES(?,?,?,?,'pending')",
                params![request.nonce, encoded, stage, pin])?;
            (stage, None)
        };
        let candidate = if let Some(candidate) = previous_candidate {
            ensure!(
                candidate.len() <= MAX_WITNESS_BYTES,
                "capture candidate exceeds bound"
            );
            let parsed: Candidate = serde_json::from_str(&candidate)?;
            ensure!(
                canonical_json(&parsed)? == candidate.as_bytes(),
                "capture candidate is not canonical"
            );
            parsed
        } else {
            if self.observation_cursor(&c)? != request.expected_begin {
                c.execute(
                    "UPDATE partial_captures SET state='unavailable' WHERE nonce=?",
                    [&request.nonce],
                )?;
                bail!("capture pending generation changed; use a new nonce");
            }
            self.capture_stage_available(&c, request, &stage, &pin)?;
            if c.query_row(
                "SELECT EXISTS(SELECT 1 FROM nodes WHERE kind=1 AND nlink>1)",
                [],
                |r| r.get::<_, bool>(0),
            )? {
                return Err(error(libc::EOPNOTSUPP))
                    .context("partial capture hardlinks need a qualified topology adapter");
            }
            let collector = Collector::new(self, &stage, &meter, &budget.delta)?;
            let tx = c.transaction()?;
            let captured =
                namespace::capture_partial_metered(self, &tx, &stage, &policy, &meter, &collector)?;
            let sequence = head(&tx)?
                .map_or(0, |h| h.1)
                .checked_add(1)
                .filter(|n| *n <= i64::MAX as u64)
                .context("capture head sequence exhausted")?;
            let candidate =
                collector.candidate(captured.directory, self.observation_cursor(&tx)?, sequence)?;
            let bytes = canonical_json(&candidate)?;
            ensure!(
                bytes.len() <= MAX_WITNESS_BYTES,
                "capture candidate witness quota exhausted"
            );
            journal_room(&tx, bytes.len())?;
            tx.execute(
                "UPDATE partial_captures SET state='candidate',candidate=? WHERE nonce=?",
                params![String::from_utf8(bytes)?, request.nonce],
            )?;
            tx.commit()?;
            candidate
        };
        ensure!(
            candidate.end == request.expected_begin
                && candidate.directory.kind == ObjectKind::Directory
                && candidate.hardlinks == *remote.topology()
                && candidate.produced.len() <= budget.delta.max_objects
                && candidate.boundaries.len() <= budget.delta.max_boundary_objects,
            "capture candidate differs from its reservation/budget"
        );
        self.capture_stage_available(&c, request, &stage, &pin)?;
        if self.manager().store().object_set_receipt(&pin)?.is_none() {
            for reference in &candidate.produced {
                meter.check()?;
                if reference.kind == ObjectKind::Blob {
                    meter.content(reference.size_bytes)?;
                } else {
                    meter.metadata(reference.size_bytes)?;
                }
                if self.manager().store().read_object(reference).is_err() {
                    c.execute(
                        "UPDATE partial_captures SET state='unavailable' WHERE nonce=?",
                        [&request.nonce],
                    )?;
                    bail!("capture candidate bytes unavailable; use a new nonce");
                }
            }
        }
        let boundaries = replay_boundaries(remote.session(), &candidate.boundaries, &meter)?;
        meter.check()?;
        let mut delta_budget = budget.delta.clone();
        delta_budget.timeout = delta_budget.timeout.min(
            meter
                .deadline()
                .saturating_duration_since(std::time::Instant::now()),
        );
        meter.check()?;
        admission.validate(self, &c, request)?;
        let retained = delta::retain_partial_tree(
            self.manager().store(),
            &stage,
            &pin,
            &request.nonce,
            remote.session(),
            &candidate.directory,
            &candidate.hardlinks,
            &candidate.produced,
            &boundaries,
            &delta_budget,
        )?;
        // Persist data retention before any late cancellation/fence/head error.
        // The original candidate remains enough to reconcile ACK loss even if
        // retain_partial_tree itself observed a fence after its ledger commit.
        let retention_json = serde_json::to_string(retained.receipt())?;
        journal_room(&c, retention_json.len())?;
        c.execute(
            "UPDATE partial_captures SET state='retained',retention=? WHERE nonce=?",
            params![retention_json, request.nonce],
        )?;
        meter.check()?;
        remote.guard()?;
        admission.validate(self, &c, request)?;
        let tx = c.transaction()?;
        ensure!(
            head(&tx)?.map(|h| h.0) == request.expected_head,
            "capture retained but head changed; reconcile this nonce"
        );
        let receipt = PartialCaptureReceipt {
            format_version: 1,
            view_id: self.id().into(),
            owner: self.0.owner.clone(),
            attempt: binding.attempt.clone(),
            base: binding.base.clone(),
            begin: request.expected_begin.clone(),
            end: candidate.end,
            exclusion_policy_digest: binding.policy,
            request_nonce: request.nonce.clone(),
            previous_head: request.expected_head.clone(),
            head_sequence: candidate.sequence,
            retention: retained.receipt().clone(),
            consistency: ObservationConsistency::LiveBestEffort,
        };
        let result = serde_json::to_string(&receipt)?;
        journal_room(&tx, result.len())?;
        tx.execute(
            "UPDATE partial_captures SET state='committed',result=? WHERE nonce=?",
            params![result, request.nonce],
        )?;
        tx.execute(
            "INSERT OR REPLACE INTO partial_capture_head VALUES(1,?,?)",
            params![request.nonce, sql_id(candidate.sequence)],
        )?;
        tx.commit()?;
        // A late fence/cancellation can withhold the reply after durable head
        // commit. Keep the exact record and all retention for reconciliation.
        meter.check()?;
        remote.guard()?;
        admission.validate(self, &c, request)?;
        Ok(receipt)
    }
    fn capture_stage_available(
        &self,
        c: &Connection,
        request: &PartialCaptureRequest,
        stage: &str,
        pin: &str,
    ) -> Result<()> {
        if let Some(receipt) = self.manager().store().object_set_receipt(pin)? {
            ensure!(
                receipt.staging_id == stage,
                "capture retention belongs to another stage"
            );
            return Ok(());
        }
        let alive = self
            .manager()
            .store()
            .retention()?
            .staging
            .get(stage)
            .is_some_and(|s| {
                s.expires_at_ms
                    > SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64
            });
        if !alive {
            c.execute(
                "UPDATE partial_captures SET state='unavailable' WHERE nonce=?",
                [&request.nonce],
            )?;
            bail!("capture candidate staging unavailable; use a new nonce");
        }
        Ok(())
    }
    fn validate_partial_receipt(
        &self,
        c: &Connection,
        binding: &Binding,
        receipt: &PartialCaptureReceipt,
    ) -> Result<()> {
        ensure!(
            receipt.format_version == 1
                && receipt.view_id == self.id()
                && receipt.owner == self.0.owner
                && receipt.attempt == binding.attempt
                && receipt.base == binding.base
                && receipt.begin == binding.request.expected_begin
                && receipt.end == receipt.begin
                && receipt.exclusion_policy_digest == binding.policy
                && receipt.request_nonce == binding.request.nonce
                && receipt.retention.local.pin_id
                    == format!("vfs:partial:{}:{}", self.id(), receipt.request_nonce)
                && receipt.previous_head == binding.request.expected_head
                && receipt.head_sequence > 0
                && receipt.consistency == ObservationConsistency::LiveBestEffort,
            "partial capture receipt identity differs"
        );
        let plan = delta::read_partial_tree(self.manager().store(), &receipt.retention)?;
        let candidate: String = c.query_row(
            "SELECT candidate FROM partial_captures WHERE nonce=?",
            [&receipt.request_nonce],
            |r| r.get(0),
        )?;
        let candidate: Candidate = serde_json::from_str(&candidate)?;
        ensure!(
            plan.capture_id == receipt.request_nonce
                && plan.base == receipt.base
                && plan.attempt == receipt.attempt
                && plan.directory == candidate.directory
                && plan.hardlinks == candidate.hardlinks
                && receipt.head_sequence == candidate.sequence,
            "capture retained plan differs from owner binding"
        );
        Ok(())
    }
    pub fn inspect_partial_capture(&self, nonce: &str) -> Result<Option<PartialCaptureStatus>> {
        canonical_nonce(nonce)?;
        let c = self.db()?;
        if !existing_schema(&c, self.manager().0.remote.as_deref())? {
            return Ok(None);
        }
        let row: Option<(String, String, Option<String>, Option<String>)> = c
            .query_row(
                "SELECT binding,state,retention,result FROM partial_captures WHERE nonce=?",
                [nonce],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        row.map(|(binding, state, retention, result)| {
            let binding: Binding = serde_json::from_str(&binding)?;
            let remote = self
                .manager()
                .0
                .remote
                .as_ref()
                .context("partial capture needs remote session")?;
            ensure!(
                binding.base == *remote.session().binding()
                    && binding.attempt == *remote.session().attempt(),
                "capture inspection belongs to another attempt"
            );
            let retention = retention
                .map(|s| serde_json::from_str::<PartialTreeReceipt>(&s))
                .transpose()?;
            let receipt = result
                .map(|s| serde_json::from_str::<PartialCaptureReceipt>(&s))
                .transpose()?;
            if state != "released" {
                if let Some(r) = &receipt {
                    self.validate_partial_receipt(&c, &binding, r)?;
                } else if let Some(r) = &retention {
                    delta::read_partial_tree(self.manager().store(), r)?;
                }
            }
            remote.guard()?;
            Ok(PartialCaptureStatus {
                state,
                retention,
                receipt,
                current: head(&c)?.is_some_and(|h| h.0 == nonce),
            })
        })
        .transpose()
    }
    pub fn partial_capture_head(&self) -> Result<Option<PartialCaptureReceipt>> {
        let nonce = {
            let c = self.db()?;
            if !existing_schema(&c, self.manager().0.remote.as_deref())? {
                return Ok(None);
            }
            head(&c)?.map(|h| h.0)
        };
        nonce
            .map(|nonce| {
                self.inspect_partial_capture(&nonce)?
                    .and_then(|s| s.receipt)
                    .context("capture head has no committed receipt")
            })
            .transpose()
    }
    pub fn release_partial_capture(&self, nonce: &str) -> Result<()> {
        let effect = self.admit_live_effect()?;
        effect.validate(self)?;
        canonical_nonce(nonce)?;
        let mut c = self.db()?;
        ensure!(
            existing_schema(&c, self.manager().0.remote.as_deref())?,
            "unknown partial capture"
        );
        let tx = c.transaction()?;
        ensure!(
            !head(&tx)?.is_some_and(|h| h.0 == nonce),
            "current partial capture head is protected"
        );
        let (stage, pin, stored, state, retention): (
            String,
            String,
            Option<String>,
            String,
            Option<String>,
        ) = tx.query_row(
            "SELECT stage,pin,release_receipt,state,retention FROM partial_captures WHERE nonce=?",
            [nonce],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )?;
        ensure!(
            pin == format!("vfs:partial:{}:{nonce}", self.id()),
            "capture release pin differs"
        );
        let release = if let Some(stored) = stored {
            Some(serde_json::from_str::<ObjectSetReceipt>(&stored)?)
        } else if state == "released" {
            None
        } else if let Some(retention) = retention {
            Some(serde_json::from_str::<PartialTreeReceipt>(&retention)?.local)
        } else {
            self.manager().store().object_set_receipt(&pin)?
        };
        if let Some(r) = &release {
            ensure!(r.staging_id == stage, "capture release stage differs");
        }
        if state != "released" {
            let body = release.as_ref().map(serde_json::to_string).transpose()?;
            journal_room(&tx, body.as_ref().map_or(0, String::len))?;
            tx.execute(
                "UPDATE partial_captures SET state='released',release_receipt=? WHERE nonce=?",
                params![body, nonce],
            )?;
        }
        tx.commit()?;
        if let Some(r) = release {
            self.manager().store().release_object_set(&r)?;
        }
        self.manager().store().abandon_staging(&stage)
    }
}
