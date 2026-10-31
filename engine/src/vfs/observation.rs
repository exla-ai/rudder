//! Retained, explicitly non-quiesced source observations. A cursor records
//! acknowledged logical mutations only. Kernel dirty mmap pages are not a
//! process/writer barrier even when the beginning and ending cursors match.
use super::*;
use std::cell::{Cell, RefCell};
use std::time::Instant;

const MAX_EVENTS: u64 = 16_384;
const MAX_PAGE: usize = 1_024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirtyCursor {
    pub view_id: String,
    pub owner: OwnerEpoch,
    pub namespace: String,
    pub generation: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationConsistency {
    LiveBestEffort,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VfsObservation {
    pub format_version: u32,
    pub view_id: String,
    pub owner: OwnerEpoch,
    pub begin: DirtyCursor,
    pub end: DirtyCursor,
    pub directory: ObjectRef,
    /// Digest of explicit subtree exclusions only, not a jj tracking policy.
    pub exclusion_policy_digest: String,
    pub request_nonce: String,
    pub pin_id: String,
    pub consistency: ObservationConsistency,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirtyEvent {
    pub generation: u64,
    pub operation: String,
    pub inode: u64,
    pub parent: Option<u64>,
    pub name_hex: Option<String>,
    pub new_parent: Option<u64>,
    pub new_name_hex: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirtyPage {
    pub events: Vec<DirtyEvent>,
    pub next: DirtyCursor,
    pub current: DirtyCursor,
    pub complete: bool,
    pub has_more: bool,
    pub cursor_lost: bool,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservationBudget {
    pub max_changed_entries: u64,
    /// Bytes admitted for immutable Blob verification, private block reads,
    /// and newly encoded chunks. Repeated private reads/writes count again.
    pub max_content_bytes: u64,
    /// Encoded metadata read/written during capture, including repeated reads.
    pub max_metadata_bytes: u64,
    /// Checked between bounded local operations, not a preemptive I/O timeout.
    pub max_elapsed_ms: u64,
}
impl Default for ObservationBudget {
    fn default() -> Self {
        Self {
            max_changed_entries: 100_000,
            max_content_bytes: 256 * 1024 * 1024,
            max_metadata_bytes: 128 * 1024 * 1024,
            max_elapsed_ms: 30_000,
        }
    }
}
pub(super) struct WorkMeter<'a> {
    budget: &'a ObservationBudget,
    cancelled: Option<&'a dyn Fn() -> bool>,
    start: Instant,
    entries: Cell<u64>,
    content: Cell<u64>,
    metadata: Cell<u64>,
    admitted_blobs: RefCell<BTreeSet<ObjectRef>>,
}
impl<'a> WorkMeter<'a> {
    pub fn new(budget: &'a ObservationBudget) -> Result<Self> {
        ensure!(
            budget.max_elapsed_ms > 0,
            "observation deadline must be positive"
        );
        Ok(Self {
            budget,
            cancelled: None,
            start: Instant::now(),
            entries: Cell::new(0),
            content: Cell::new(0),
            metadata: Cell::new(0),
            admitted_blobs: RefCell::new(BTreeSet::new()),
        })
    }
    pub fn cancellable(
        budget: &'a ObservationBudget,
        cancelled: &'a dyn Fn() -> bool,
    ) -> Result<Self> {
        let mut meter = Self::new(budget)?;
        meter.cancelled = Some(cancelled);
        meter.check()?;
        Ok(meter)
    }
    pub fn check(&self) -> Result<()> {
        ensure!(
            !self.cancelled.is_some_and(|cancelled| cancelled()),
            "source observation cancelled"
        );
        ensure!(
            self.start.elapsed().as_millis() < u128::from(self.budget.max_elapsed_ms),
            "source observation exceeded its cooperative deadline"
        );
        Ok(())
    }
    pub fn deadline(&self) -> Instant {
        self.start + Duration::from_millis(self.budget.max_elapsed_ms)
    }
    fn charge(&self, counter: &Cell<u64>, count: u64, maximum: u64, label: &str) -> Result<()> {
        self.check()?;
        let next = counter
            .get()
            .checked_add(count)
            .context("observation work counter overflow")?;
        ensure!(next <= maximum, "source observation exceeds {label} budget");
        counter.set(next);
        Ok(())
    }
    pub fn entry(&self) -> Result<()> {
        self.charge(
            &self.entries,
            1,
            self.budget.max_changed_entries,
            "changed-entry",
        )
    }
    pub fn metadata(&self, bytes: u64) -> Result<()> {
        self.charge(
            &self.metadata,
            bytes,
            self.budget.max_metadata_bytes,
            "metadata",
        )
    }
    pub fn content(&self, bytes: u64) -> Result<()> {
        self.charge(
            &self.content,
            bytes,
            self.budget.max_content_bytes,
            "content",
        )
    }
    pub fn blob(&self, reference: &ObjectRef) -> Result<()> {
        if !self.admitted_blobs.borrow().contains(reference) {
            self.content(reference.size_bytes)?;
            self.admitted_blobs.borrow_mut().insert(reference.clone());
        }
        self.check()
    }
}

pub(super) fn initialize(c: &Connection) -> Result<()> {
    initialize_reserved(c, &Uuid::new_v4().to_string())
}
pub(super) fn initialize_reserved(c: &Connection, namespace: &str) -> Result<()> {
    ensure!(
        Uuid::parse_str(namespace)?.to_string() == namespace,
        "observation namespace must be canonical UUID"
    );
    c.execute_batch("CREATE TABLE observation_state(singleton INTEGER PRIMARY KEY CHECK(singleton=1),namespace TEXT NOT NULL,generation INTEGER NOT NULL CHECK(generation>=0));CREATE TABLE dirty_events(generation INTEGER PRIMARY KEY,body TEXT NOT NULL);CREATE TABLE observations(nonce TEXT PRIMARY KEY,policy TEXT NOT NULL,generation INTEGER NOT NULL,namespace TEXT NOT NULL,pin TEXT NOT NULL,status TEXT NOT NULL,result TEXT);")?;
    c.execute("INSERT INTO observation_state VALUES(1,?,0)", [namespace])?;
    Ok(())
}
pub(super) fn mutation(
    c: &Connection,
    operation: &str,
    inode: u64,
    parent: Option<u64>,
    name: Option<&[u8]>,
    destination: Option<(u64, &[u8])>,
) -> Result<()> {
    if !namespace::enabled(c)? {
        return Ok(());
    }
    ensure!(
        !c.is_autocommit(),
        "dirty generation requires mutation transaction"
    );
    let old = c.query_row(
        "SELECT generation FROM observation_state WHERE singleton=1",
        [],
        |r| read_id(r, 0),
    )?;
    let generation = old
        .checked_add(1)
        .filter(|n| *n <= i64::MAX as u64)
        .context("dirty generation exhausted")?;
    let event = DirtyEvent {
        generation,
        operation: operation.into(),
        inode,
        parent,
        name_hex: name.map(hex::encode),
        new_parent: destination.map(|d| d.0),
        new_name_hex: destination.map(|d| hex::encode(d.1)),
    };
    c.execute(
        "UPDATE observation_state SET generation=? WHERE singleton=1",
        [sql_id(generation)],
    )?;
    c.execute(
        "INSERT INTO dirty_events VALUES(?,?)",
        params![sql_id(generation), serde_json::to_string(&event)?],
    )?;
    c.execute(
        "DELETE FROM dirty_events WHERE generation<=?",
        [sql_id(generation.saturating_sub(MAX_EVENTS))],
    )?;
    Ok(())
}

pub(super) fn normalized_policy(policy: &ImportPolicy) -> Result<(ImportPolicy, String)> {
    ensure!(
        policy.excluded_paths.len() <= 4096,
        "source exclusion count exceeds4096"
    );
    let mut paths = policy.excluded_paths.clone();
    for path in &paths {
        ensure!(
            !path.is_empty() && path.len() <= MAX_DEPTH,
            "invalid source exclusion depth"
        );
        for part in path {
            name_ok(part)?;
        }
    }
    paths.sort();
    paths.dedup();
    let mut minimal: Vec<Vec<Vec<u8>>> = Vec::new();
    for path in paths {
        if minimal
            .last()
            .is_none_or(|ancestor| !path.starts_with(ancestor))
        {
            minimal.push(path);
        }
    }
    let encoded = minimal
        .iter()
        .map(|path| path.iter().map(hex::encode).collect::<Vec<_>>())
        .collect::<Vec<_>>();
    let bytes =
        canonical_json(&serde_json::json!({"format_version":1,"excluded_paths_hex":encoded}))?;
    ensure!(
        bytes.len() <= 4 * 1024 * 1024,
        "source exclusion metadata exceeds4MiB"
    );
    Ok((
        ImportPolicy {
            excluded_paths: minimal,
        },
        hex::encode(Sha256::digest(bytes)),
    ))
}
pub fn exclusion_policy_digest(policy: &ImportPolicy) -> Result<String> {
    Ok(normalized_policy(policy)?.1)
}

impl VfsView {
    pub(super) fn observation_cursor(&self, c: &Connection) -> Result<DirtyCursor> {
        ensure!(
            self.0.v2,
            "source observations require explicit paged/range profile"
        );
        let (namespace, generation) = c.query_row(
            "SELECT namespace,generation FROM observation_state WHERE singleton=1",
            [],
            |r| Ok((r.get::<_, String>(0)?, read_id(r, 1)?)),
        )?;
        ensure!(
            Uuid::parse_str(&namespace).is_ok(),
            "invalid dirty cursor namespace"
        );
        Ok(DirtyCursor {
            view_id: self.id().into(),
            owner: self.0.owner.clone(),
            namespace,
            generation,
        })
    }
    pub fn dirty_cursor(&self) -> Result<DirtyCursor> {
        self.observation_cursor(&*self.db()?)
    }
    pub fn dirty_since(&self, cursor: &DirtyCursor, limit: usize) -> Result<DirtyPage> {
        ensure!(
            (1..=MAX_PAGE).contains(&limit),
            "dirty page limit must be1..=1024"
        );
        let c = self.db()?;
        let current = self.observation_cursor(&c)?;
        let oldest = c.query_row("SELECT min(generation) FROM dirty_events", [], |r| {
            r.get::<_, Option<i64>>(0)
        })?;
        let lost = cursor.view_id != current.view_id
            || cursor.owner != current.owner
            || cursor.namespace != current.namespace
            || cursor.generation > current.generation
            || (cursor.generation < current.generation
                && oldest.is_none_or(|old| cursor.generation < old.saturating_sub(1) as u64));
        if lost {
            return Ok(DirtyPage {
                events: Vec::new(),
                next: current.clone(),
                current,
                complete: false,
                has_more: false,
                cursor_lost: true,
            });
        }
        let mut q = c.prepare(
            "SELECT generation,body FROM dirty_events WHERE generation>? ORDER BY generation LIMIT ?",
        )?;
        let mut events = q
            .query_map(
                params![sql_id(cursor.generation), (limit + 1) as i64],
                |r| Ok((read_id(r, 0)?, r.get::<_, String>(1)?)),
            )?
            .map(|row| {
                let (generation, body) = row?;
                let event = serde_json::from_str::<DirtyEvent>(&body)?;
                ensure!(
                    event.generation == generation,
                    "dirty event generation differs from its record"
                );
                Ok(event)
            })
            .collect::<Result<Vec<_>>>()?;
        let mut expected = cursor.generation;
        for event in &events {
            expected = expected
                .checked_add(1)
                .context("dirty generation overflow")?;
            ensure!(
                event.generation == expected,
                "dirty history has an interior gap"
            );
        }
        let more = events.len() > limit;
        events.truncate(limit);
        let mut next = current.clone();
        next.generation = events.last().map_or(cursor.generation, |e| e.generation);
        ensure!(
            more || next.generation == current.generation,
            "dirty history ends before current generation"
        );
        Ok(DirtyPage {
            events,
            next,
            current,
            complete: !more,
            has_more: more,
            cursor_lost: false,
        })
    }
    /// Owner-only observation. Does not unmount, stop writers, clear dirty state,
    /// replace a root, or mint a Quiesced/terminal checkpoint.
    pub fn observe_source(
        &self,
        policy: &ImportPolicy,
        request_nonce: &str,
        budget: &ObservationBudget,
    ) -> Result<VfsObservation> {
        self.observe_source_cancellable(policy, request_nonce, budget, &|| false)
    }

    /// Cooperative cancellation is checked between bounded local operations.
    /// It cannot preempt kernel I/O or erase a publication whose outcome became
    /// uncertain; the caller inspects the original nonce before retrying.
    pub fn observe_source_cancellable(
        &self,
        policy: &ImportPolicy,
        request_nonce: &str,
        budget: &ObservationBudget,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<VfsObservation> {
        self.require_local_checkpoint()?;
        ensure!(
            Uuid::parse_str(request_nonce).is_ok(),
            "observation nonce must be a UUID"
        );
        let (policy, digest) = normalized_policy(policy)?;
        let meter = WorkMeter::cancellable(budget, cancelled)?;
        let mut c = self.db()?;
        meter.check()?;
        mutable(&c)?;
        let begin = self.observation_cursor(&c)?;
        let pin = format!("vfs:observe:{}:{request_nonce}", self.id());
        let existing=c.query_row("SELECT policy,generation,namespace,pin,status,result FROM observations WHERE nonce=?",[request_nonce],|r|Ok((r.get::<_,String>(0)?,read_id(r,1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?,r.get::<_,Option<String>>(5)?))).optional()?;
        if let Some((policy, generation, namespace, stored_pin, status, result)) = existing {
            ensure!(
                policy == digest && stored_pin == pin && namespace == begin.namespace,
                "observation nonce bound to different request"
            );
            ensure!(status != "released", "observation nonce has been retired");
            ensure!(
                status == "pending" || status == "complete",
                "invalid observation receipt status"
            );
            if let Some(result) = result {
                let result: VfsObservation = serde_json::from_str(&result)?;
                result.directory.validate()?;
                ensure!(
                    status == "complete"
                        && result.format_version == 1
                        && result.view_id == self.id()
                        && result.owner == self.0.owner
                        && result.begin == result.end
                        && result.begin.view_id == self.id()
                        && result.begin.owner == self.0.owner
                        && result.begin.namespace == namespace
                        && result.begin.generation == generation
                        && result.exclusion_policy_digest == digest
                        && result.request_nonce == request_nonce
                        && result.pin_id == pin
                        && result.directory.kind == ObjectKind::Directory,
                    "observation receipt identity differs from reservation"
                );
                ensure!(
                    self.manager().store().retention()?.pins.get(&pin) == Some(&result.directory),
                    "observation retention missing"
                );
                return Ok(result);
            }
            if let Some(directory) = self.manager().store().retention()?.pins.get(&pin).cloned() {
                ensure!(
                    directory.kind == ObjectKind::Directory,
                    "pending observation pin is not a Directory"
                );
                crate::tree::root_info(self.manager().store(), &directory)?;
                // Only the owning process can publish this reserved pin. It held
                // the DB mutex through capture. Recovery preserves its original
                // generation even if later writes preceded this retry.
                let mut cursor = begin.clone();
                cursor.generation = generation;
                let result = VfsObservation {
                    format_version: 1,
                    view_id: self.id().into(),
                    owner: self.0.owner.clone(),
                    begin: cursor.clone(),
                    end: cursor,
                    directory,
                    exclusion_policy_digest: digest,
                    request_nonce: request_nonce.into(),
                    pin_id: pin,
                    consistency: ObservationConsistency::LiveBestEffort,
                };
                c.execute(
                    "UPDATE observations SET status='complete',result=? WHERE nonce=?",
                    params![serde_json::to_string(&result)?, request_nonce],
                )?;
                return Ok(result);
            }
            ensure!(
                generation == begin.generation,
                "observation outcome uncertain after mutations; use a new nonce"
            );
        } else {
            c.execute(
                "INSERT INTO observations VALUES(?,?,?,?,?,'pending',NULL)",
                params![
                    request_nonce,
                    digest,
                    sql_id(begin.generation),
                    begin.namespace,
                    pin
                ],
            )?;
        }
        let stage = self
            .manager()
            .store()
            .begin_staging(Duration::from_secs(3600))?;
        let outcome = (|| {
            let tx = c.transaction()?;
            let captured = namespace::capture_metered(self, &tx, &stage.id, &policy, Some(&meter))?;
            meter.check()?;
            let anchors = namespace::retained_anchors(self, &tx)?;
            self.manager().store().publish_extending(
                &stage.id,
                &pin,
                &captured.directory,
                &anchors,
            )?;
            let end = self.observation_cursor(&tx)?;
            let result = VfsObservation {
                format_version: 1,
                view_id: self.id().into(),
                owner: self.0.owner.clone(),
                begin: begin.clone(),
                end,
                directory: captured.directory,
                exclusion_policy_digest: digest,
                request_nonce: request_nonce.into(),
                pin_id: pin,
                consistency: ObservationConsistency::LiveBestEffort,
            };
            tx.execute(
                "UPDATE observations SET status='complete',result=? WHERE nonce=?",
                params![serde_json::to_string(&result)?, request_nonce],
            )?;
            tx.commit()?;
            Ok(result)
        })();
        let cleanup = self.manager().store().abandon_staging(&stage.id);
        match outcome {
            Ok(result) => {
                cleanup?;
                Ok(result)
            }
            Err(error) => Err(error),
        }
    }
    pub fn release_observation(&self, request_nonce: &str) -> Result<()> {
        ensure!(
            Uuid::parse_str(request_nonce).is_ok(),
            "observation nonce must be a UUID"
        );
        let c = self.db()?;
        self.observation_cursor(&c)?;
        let pin = c
            .query_row(
                "SELECT pin FROM observations WHERE nonce=?",
                [request_nonce],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .context("unknown observation")?;
        ensure!(
            pin == format!("vfs:observe:{}:{request_nonce}", self.id()),
            "observation pin identity differs"
        );
        // Durable retirement first; failure leaks retention instead of reviving
        // a released nonce or dropping another caller's pin.
        c.execute(
            "UPDATE observations SET status='released' WHERE nonce=?",
            [request_nonce],
        )?;
        self.manager().store().unpin(&pin)
    }
}
