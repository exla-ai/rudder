//! Logical effect exclusion for namespace5. This is not a process, RPC or
//! kernel-drain witness. Persisted intents never reconstruct an owned guard.
use super::*;
use std::sync::{Condvar, Weak};
use std::time::Instant;

const MAX_INTENT_BYTES: usize = 64 * 1024;
// Freeze must not inherit the ordinary connection's 10-second busy wait. The
// actual mutex guard keeps this temporary handler private to this operation.
struct FreezeConnection<'a> {
    connection: std::sync::MutexGuard<'a, Connection>,
    prior_busy: Duration,
}
impl<'a> FreezeConnection<'a> {
    fn new(connection: std::sync::MutexGuard<'a, Connection>) -> Result<Self> {
        let milliseconds: u64 =
            connection.query_row("PRAGMA busy_timeout", [], |row| read_id(row, 0))?;
        connection.busy_timeout(Duration::ZERO)?;
        Ok(Self {
            connection,
            prior_busy: Duration::from_millis(milliseconds),
        })
    }
}
impl std::ops::Deref for FreezeConnection<'_> {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        &self.connection
    }
}
impl std::ops::DerefMut for FreezeConnection<'_> {
    fn deref_mut(&mut self) -> &mut Connection {
        &mut self.connection
    }
}
impl Drop for FreezeConnection<'_> {
    fn drop(&mut self) {
        let _ = self.connection.busy_timeout(self.prior_busy);
    }
}
#[derive(Clone)]
pub(super) struct Admission(Arc<Gate>);
struct Gate {
    enabled: bool,
    state: Mutex<State>,
    changed: Condvar,
}
struct State {
    phase: Phase,
    effects: usize,
}
enum Phase {
    Live,
    Closing(String),
    Frozen { digest: String, owner: Weak<()> },
}
pub(super) struct LiveEffect {
    gate: Arc<Gate>,
    counted: bool,
}
impl Drop for LiveEffect {
    fn drop(&mut self) {
        if self.counted {
            if let Ok(mut state) = self.gate.state.lock() {
                state.effects -= 1;
                self.gate.changed.notify_all();
            }
        }
    }
}
impl LiveEffect {
    pub(super) fn validate(&self, view: &VfsView) -> Result<()> {
        ensure!(
            Arc::ptr_eq(&self.gate, &view.0.admission.0),
            "effect belongs to another view"
        );
        Ok(())
    }
}
impl Admission {
    pub(super) fn new(enabled: bool) -> Self {
        Self(Arc::new(Gate {
            enabled,
            state: Mutex::new(State {
                phase: Phase::Live,
                effects: 0,
            }),
            changed: Condvar::new(),
        }))
    }
    fn admit(&self, cleanup: bool) -> Result<LiveEffect> {
        if !self.0.enabled {
            return Ok(LiveEffect {
                gate: self.0.clone(),
                counted: false,
            });
        }
        let mut state = self.0.state.lock().map_err(|_| error(libc::EIO))?;
        if !matches!(state.phase, Phase::Live)
            && !(cleanup && matches!(state.phase, Phase::Closing(_)))
        {
            return Err(error(libc::EROFS));
        }
        if state.effects >= 1024 {
            return Err(error(libc::EAGAIN));
        }
        state.effects += 1;
        Ok(LiveEffect {
            gate: self.0.clone(),
            counted: true,
        })
    }
    fn close(&self, digest: &str, deadline: Instant) -> Result<()> {
        if !self.0.enabled {
            return Err(error(libc::EOPNOTSUPP))
                .context("remote freeze requires finalizable namespace5");
        }
        let mut state = self.0.state.lock().map_err(|_| error(libc::EIO))?;
        match &state.phase {
            Phase::Live => state.phase = Phase::Closing(digest.into()),
            Phase::Closing(saved) => {
                ensure!(saved == digest, "freeze already bound to another intent")
            }
            Phase::Frozen { .. } => {
                return Err(error(libc::EROFS)).context("frozen view is recovery-only");
            }
        }
        while state.effects != 0 {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or_else(|| error(libc::ETIMEDOUT))?;
            state = self
                .0
                .changed
                .wait_timeout(state, remaining)
                .map_err(|_| error(libc::EIO))?
                .0;
        }
        if Instant::now() >= deadline {
            return Err(error(libc::ETIMEDOUT));
        }
        Ok(())
    }
    fn commit(&self, digest: &str) -> Result<FrozenLease> {
        let mut state = self.0.state.lock().map_err(|_| error(libc::EIO))?;
        ensure!(
            matches!(&state.phase, Phase::Closing(saved) if saved == digest) && state.effects == 0,
            "freeze admission changed"
        );
        let owner = Arc::new(());
        state.phase = Phase::Frozen {
            digest: digest.into(),
            owner: Arc::downgrade(&owner),
        };
        Ok(FrozenLease {
            gate: self.0.clone(),
            owner,
            digest: digest.into(),
        })
    }
}
struct FrozenLease {
    gate: Arc<Gate>,
    owner: Arc<()>,
    digest: String,
}
impl FrozenLease {
    fn validate(&self, view: &VfsView) -> Result<()> {
        ensure!(
            Arc::ptr_eq(&self.gate, &view.0.admission.0),
            "frozen guard belongs to another view"
        );
        let state = self.gate.state.lock().map_err(|_| error(libc::EIO))?;
        ensure!(
            matches!(&state.phase, Phase::Frozen { digest, owner }
            if digest == &self.digest && owner.upgrade().is_some_and(|owner| Arc::ptr_eq(&owner, &self.owner))),
            "actual frozen guard is unavailable"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteFreezeRequest {
    pub format_version: u32,
    pub intent_id: String,
    pub capture: PartialCaptureRequest,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteFreezeIntent {
    pub format_version: u32,
    pub request: RemoteFreezeRequest,
    pub launch_digest: String,
}
fn validate_request(
    request: &RemoteFreezeRequest,
    launch: &remote_launch::RemoteLaunchRequest,
) -> Result<()> {
    ensure!(request.format_version == 1, "unknown remote freeze request");
    for id in [&request.intent_id, &request.capture.nonce] {
        ensure!(
            Uuid::parse_str(id)?.to_string() == *id,
            "freeze identity must be canonical UUID"
        );
    }
    if let Some(head) = &request.capture.expected_head {
        ensure!(
            Uuid::parse_str(head)?.to_string() == *head,
            "freeze head must be canonical UUID"
        );
    }
    let begin = &request.capture.expected_begin;
    ensure!(
        begin.view_id == launch.attempt.view_id
            && begin.owner == launch.attempt.owner
            && begin.namespace == launch.observation.namespace
            && begin.generation <= i64::MAX as u64,
        "freeze cursor differs from the exact launch binding"
    );
    Ok(())
}
pub(super) fn initialize(c: &Connection) -> Result<()> {
    c.execute_batch("CREATE TABLE remote_freeze_intent(singleton INTEGER PRIMARY KEY CHECK(singleton=1),body TEXT NOT NULL);")?;
    Ok(())
}
pub(super) fn validate_schema(
    c: &Connection,
    remote: &remote::Resolver,
) -> Result<Option<RemoteFreezeIntent>> {
    let launch = remote
        .launch_request()
        .context("namespace5 requires strict launch binding")?;
    let namespace: String = c.query_row(
        "SELECT namespace FROM observation_state WHERE singleton=1",
        [],
        |r| r.get(0),
    )?;
    let binding: String = c.query_row(
        "SELECT value FROM meta WHERE key='remote_binding'",
        [],
        |r| r.get(0),
    )?;
    remote.persisted_binding(&binding, &namespace)?;
    let view_id: String = c.query_row("SELECT value FROM meta WHERE key='view_id'", [], |r| {
        r.get(0)
    })?;
    let owner: String =
        c.query_row("SELECT value FROM meta WHERE key='owner'", [], |r| r.get(0))?;
    let base: String = c.query_row("SELECT value FROM meta WHERE key='base'", [], |r| r.get(0))?;
    ensure!(
        view_id == launch.attempt.view_id
            && serde_json::from_str::<OwnerEpoch>(&owner)? == launch.attempt.owner
            && namespace == launch.observation.namespace
            && serde_json::from_str::<ObjectRef>(&base)? == *remote.base(),
        "finalizable namespace identity changed"
    );
    let count: u64 = c.query_row("SELECT count(*) FROM remote_freeze_intent", [], |r| {
        read_id(r, 0)
    })?;
    ensure!(count <= 1, "invalid remote freeze journal");
    let row: Option<(u64, Option<String>)> = c
        .query_row(
            "SELECT length(CAST(body AS BLOB)),CASE WHEN length(CAST(body AS BLOB))<=65536 THEN body ELSE NULL END FROM remote_freeze_intent WHERE singleton=1",
            [],
            |r| Ok((read_id(r, 0)?, r.get(1)?)),
        )
        .optional()?;
    ensure!(
        count == u64::from(row.is_some()),
        "freeze journal singleton missing"
    );
    let body = row
        .map(|(length, body)| {
            if length > MAX_INTENT_BYTES as u64 {
                return Err(error(libc::EFBIG));
            }
            body.context("freeze intent missing")
        })
        .transpose()?;
    let marker: Option<String> = c
        .query_row(
            "SELECT value FROM meta WHERE key='remote_frozen'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    ensure!(
        (body.is_some() && marker.as_deref() == Some("true"))
            || (body.is_none() && marker.is_none()),
        "incomplete remote freeze journal; recovery required"
    );
    body.map(|body| {
        ensure!(
            body.len() <= MAX_INTENT_BYTES,
            "freeze intent exceeds 64 KiB"
        );
        let intent: RemoteFreezeIntent = serde_json::from_str(&body)?;
        ensure!(
            intent.format_version == 1 && canonical_json(&intent)? == body.as_bytes(),
            "invalid freeze intent"
        );
        validate_request(&intent.request, launch)?;
        ensure!(
            intent.launch_digest == hex::encode(Sha256::digest(canonical_json(launch)?)),
            "freeze launch binding differs"
        );
        let generation: u64 = c.query_row(
            "SELECT generation FROM observation_state WHERE singleton=1",
            [],
            |r| read_id(r, 0),
        )?;
        ensure!(
            generation == intent.request.capture.expected_begin.generation,
            "frozen dirty generation changed"
        );
        let head = capture::head(c)?.map(|h| h.0);
        let committed: bool = c.query_row(
            "SELECT EXISTS(SELECT 1 FROM partial_captures WHERE nonce=? AND state='committed')",
            [&intent.request.capture.nonce],
            |row| row.get(0),
        )?;
        let expected_head = if committed {
            Some(intent.request.capture.nonce.clone())
        } else {
            intent.request.capture.expected_head.clone()
        };
        ensure!(head == expected_head, "frozen capture head changed");
        Ok(intent)
    })
    .transpose()
}

/// Owns logical exclusion and actual view resources, never physical quiescence.
/// Drop leaves the view frozen. Disk records cannot reconstruct this guard.
pub struct FrozenRemoteView {
    view: VfsView,
    lease: FrozenLease,
    intent: RemoteFreezeIntent,
    bytes: Vec<u8>,
    captured: Option<PartialCaptureReceipt>,
}
impl FrozenRemoteView {
    pub fn intent(&self) -> &RemoteFreezeIntent {
        &self.intent
    }
    pub fn intent_bytes(&self) -> &[u8] {
        &self.bytes
    }
    /// Exact immutable identity only; the caller must retain these bytes.
    pub fn intent_reference(&self) -> ObjectRef {
        ObjectRef::from_bytes(ObjectKind::Blob, &self.bytes)
    }
    /// Current logical ownership/data identity only, never physical quiescence.
    pub fn check_current(&self) -> Result<()> {
        let c = self.view.db()?;
        self.validate(&self.view, &c, &self.intent.request.capture)?;
        if let Some(receipt) = &self.captured {
            let result: Option<String> = c.query_row("SELECT CASE WHEN length(CAST(result AS BLOB))<=65536 THEN result ELSE NULL END FROM partial_captures WHERE nonce=? AND state='committed'", [&receipt.request_nonce], |row| row.get(0))?;
            ensure!(
                serde_json::from_str::<PartialCaptureReceipt>(
                    &result.context("frozen capture result missing/oversized")?
                )? == *receipt,
                "acknowledged frozen capture changed"
            );
        }
        Ok(())
    }
    /// Bind this actual owner to the executor's actual workspace view. Equal
    /// request/cursor data under another manager/store is insufficient.
    pub fn check_for(&self, view: &VfsView) -> Result<()> {
        ensure!(
            Arc::ptr_eq(&self.view.0, &view.0),
            "frozen owner belongs to another actual view"
        );
        self.lease.validate(view)?;
        self.check_current()
    }
    pub(super) fn validate(
        &self,
        view: &VfsView,
        c: &Connection,
        request: &PartialCaptureRequest,
    ) -> Result<()> {
        self.lease.validate(view)?;
        check_custody(view)?;
        ensure!(
            request == &self.intent.request.capture,
            "capture differs from frozen intent"
        );
        ensure!(
            validate_schema(
                c,
                view.manager()
                    .0
                    .remote
                    .as_deref()
                    .context("remote resolver absent")?
            )?
            .as_ref()
                == Some(&self.intent),
            "durable freeze intent changed"
        );
        Ok(())
    }
    /// Still LiveBestEffort data ACK. Only the executor may join this with its
    /// actual process/application/RPC/FUSE drain and durable execution record.
    pub fn capture(
        &mut self,
        budget: &PartialCaptureBudget,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<PartialCaptureReceipt> {
        let receipt = self
            .view
            .capture_partial_source_frozen(self, budget, cancelled)?;
        ensure!(
            self.captured.as_ref().is_none_or(|saved| saved == &receipt),
            "frozen capture result changed"
        );
        self.captured = Some(receipt.clone());
        Ok(receipt)
    }
}
fn check_custody(view: &VfsView) -> Result<()> {
    let held = view.0._lock.metadata()?;
    let named = fs::symlink_metadata(view.0.path.join("owner.lock"))?;
    ensure!(
        held.is_file()
            && named.is_file()
            && !named.file_type().is_symlink()
            && held.dev() == named.dev()
            && held.ino() == named.ino()
            && held.uid() == unsafe { libc::geteuid() }
            && held.nlink() == 1
            && held.mode() & 0o077 == 0,
        "actual view lock custody changed"
    );
    Ok(())
}
impl VfsView {
    pub(super) fn admit_live_effect(&self) -> Result<LiveEffect> {
        self.0.admission.admit(false)
    }
    pub(super) fn admit_cleanup_effect(&self) -> Result<LiveEffect> {
        self.0.admission.admit(true)
    }
    pub fn remote_launch_request(&self) -> Result<&remote_launch::RemoteLaunchRequest> {
        let remote = self
            .manager()
            .0
            .remote
            .as_ref()
            .ok_or_else(|| error(libc::EOPNOTSUPP))?;
        remote.guard()?;
        remote
            .launch_request()
            .ok_or_else(|| error(libc::EOPNOTSUPP))
    }
    pub fn remote_launch_digest(&self) -> Result<String> {
        Ok(hex::encode(Sha256::digest(canonical_json(
            self.remote_launch_request()?,
        )?)))
    }
    pub fn freeze_remote(
        &self,
        request: &RemoteFreezeRequest,
        timeout: Duration,
    ) -> Result<FrozenRemoteView> {
        ensure!(
            !timeout.is_zero() && timeout <= Duration::from_secs(120),
            "invalid freeze deadline"
        );
        let launch = self.remote_launch_request()?;
        validate_request(request, launch)?;
        let intent = RemoteFreezeIntent {
            format_version: 1,
            request: request.clone(),
            launch_digest: self.remote_launch_digest()?,
        };
        let bytes = canonical_json(&intent)?;
        ensure!(
            bytes.len() <= MAX_INTENT_BYTES,
            "freeze intent exceeds 64 KiB"
        );
        let digest = hex::encode(Sha256::digest(&bytes));
        let deadline = Instant::now() + timeout;
        self.0.admission.close(&digest, deadline)?;
        let c = loop {
            self.remote_launch_request()?;
            if Instant::now() >= deadline {
                return Err(error(libc::ETIMEDOUT));
            }
            match self.0.db.try_lock() {
                Ok(c) => break c,
                Err(std::sync::TryLockError::Poisoned(_)) => return Err(error(libc::EIO)),
                Err(std::sync::TryLockError::WouldBlock) => {
                    std::thread::sleep(Duration::from_millis(1))
                }
            }
        };
        let mut c = FreezeConnection::new(c)?;
        check_custody(self)?;
        if self.0.mounted.load(Ordering::Acquire) {
            return Err(error(libc::EBUSY)).context("freeze requires actual drained mount");
        }
        if !self
            .0
            .handles
            .lock()
            .map_err(|_| error(libc::EIO))?
            .is_empty()
        {
            return Err(error(libc::EBUSY)).context("freeze requires closed logical handles");
        }
        let tx = c.transaction()?;
        mutable(&tx)?;
        ensure!(
            validate_schema(
                &tx,
                self.manager()
                    .0
                    .remote
                    .as_deref()
                    .context("remote resolver absent")?
            )?
            .is_none(),
            "persisted freeze is recovery-only"
        );
        ensure!(
            self.observation_cursor(&tx)? == request.capture.expected_begin,
            "freeze cursor changed"
        );
        ensure!(
            capture::head(&tx)?.map(|h| h.0) == request.capture.expected_head,
            "freeze head changed"
        );
        tx.execute(
            "INSERT INTO remote_freeze_intent VALUES(1,?)",
            [std::str::from_utf8(&bytes)?],
        )?;
        tx.execute("INSERT INTO meta VALUES('remote_frozen','true')", [])?;
        self.remote_launch_request()?;
        if Instant::now() >= deadline {
            return Err(error(libc::ETIMEDOUT));
        }
        tx.commit()?;
        check_custody(self)?;
        let lease = self.0.admission.commit(&digest)?;
        Ok(FrozenRemoteView {
            view: self.clone(),
            lease,
            intent,
            bytes,
            captured: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn closing_drains_admitted_effects_and_cleanup_without_recursive_reader_locks() {
        let admission = Admission::new(true);
        let effect = admission.admit(false).unwrap();
        let closing = admission.clone();
        let wait = std::thread::spawn(move || {
            closing.close("intent", Instant::now() + Duration::from_millis(30))
        });
        while matches!(admission.0.state.lock().unwrap().phase, Phase::Live) {
            std::thread::yield_now();
        }
        assert!(admission.admit(false).is_err());
        // Cleanup can settle an existing handle while admission is closed.
        let cleanup = admission.admit(true).unwrap();
        drop(cleanup);
        assert_eq!(
            wait.join()
                .unwrap()
                .unwrap_err()
                .downcast_ref::<std::io::Error>()
                .unwrap()
                .raw_os_error(),
            Some(libc::ETIMEDOUT)
        );
        drop(effect);
        admission
            .close("intent", Instant::now() + Duration::from_secs(1))
            .unwrap();
        let owner = admission.commit("intent").unwrap();
        assert!(admission.admit(false).is_err());
        assert!(admission.admit(true).is_err());
        drop(owner);
        // Neither knowing the digest nor dropping the owner thaws/reconstructs.
        assert!(admission
            .close("intent", Instant::now() + Duration::from_secs(1))
            .is_err());
        assert!(admission.commit("intent").is_err());
    }
    #[test]
    fn freeze_connection_restores_ordinary_busy_handler_on_success_and_error() {
        let connection = Mutex::new(Connection::open_in_memory().unwrap());
        connection
            .lock()
            .unwrap()
            .busy_timeout(Duration::from_secs(10))
            .unwrap();
        fn operation(connection: std::sync::MutexGuard<'_, Connection>, fail: bool) -> Result<()> {
            let c = FreezeConnection::new(connection)?;
            assert_eq!(
                c.query_row("PRAGMA busy_timeout", [], |row| read_id(row, 0))?,
                0
            );
            if fail {
                bail!("fixture early failure");
            }
            Ok(())
        }
        for fail in [false, true] {
            assert_eq!(operation(connection.lock().unwrap(), fail).is_err(), fail);
            assert_eq!(
                connection
                    .lock()
                    .unwrap()
                    .query_row("PRAGMA busy_timeout", [], |row| read_id(row, 0))
                    .unwrap(),
                10_000
            );
        }
    }
}
