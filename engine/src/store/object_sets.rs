//! Retention of explicit local objects, independent of complete root pins.
//!
//! Partial workspace metadata may reference content available only remotely.
//! These receipts retain exact local bytes; they never prove root membership,
//! child semantics, a complete closure, writer quiescence or replication.
//! Every mutation/GC shares LocalStore's existing interprocess lock. A FULL
//! SQLite commit precedes removal of staging, so crashes conservatively retain.
//! Enabling this ledger permanently upgrades retention.json to schema2: older
//! clients reject it before sweeping. Normal stores remain byte-compatible v1.
use super::*;
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, TransactionBehavior};
use std::time::Instant;

const LEDGER: &str = "object-sets.sqlite";
const MAX_OBJECTS: usize = 16_384;
const MAX_RECORDS: usize = 4096;
const MAX_TOTAL_REFERENCES: usize = 131_072;
const MAX_SET_JSON: usize = 4 * 1024 * 1024;
const MAX_SET_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const VALIDATION_BUDGET: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObjectSetReceipt {
    pub format_version: u32,
    pub pin_id: String,
    pub staging_id: String,
    pub set_digest: String,
    pub generation: u64,
    pub object_count: u64,
    pub total_bytes: u64,
}

fn path(store: &LocalStore) -> PathBuf {
    store.root.join(LEDGER)
}
pub(super) fn validate_presence(store: &LocalStore) -> Result<()> {
    let metadata = fs::symlink_metadata(path(store))?;
    ensure!(
        metadata.is_file(),
        "explicit retention ledger is absent or unsafe"
    );
    #[cfg(unix)]
    ensure!(
        metadata.nlink() == 1
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0,
        "explicit retention ledger permissions/owner differ"
    );
    Ok(())
}
fn connect(store: &LocalStore) -> Result<Connection> {
    validate_presence(store)?;
    ensure!(
        rusqlite::version_number() >= 3_051_003,
        "SQLite >=3.51.3 required for retention WAL"
    );
    let connection = Connection::open_with_flags(
        path(store),
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    connection.busy_timeout(Duration::from_secs(5))?;
    connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA mmap_size=0; PRAGMA max_page_count=16384;")?;
    validate_presence(store)?;
    Ok(connection)
}
fn initialize(store: &LocalStore, state: &mut RetentionState) -> Result<()> {
    if state.format_version == 2 {
        return Ok(());
    }
    ensure!(state.format_version == 1, "unsupported retention schema");
    if !path(store).try_exists()? {
        open_file(&path(store), true, true)?.sync_all()?;
        sync_directory(&store.root)?;
    }
    let connection = connect(store)?;
    connection.execute_batch("CREATE TABLE IF NOT EXISTS object_set_meta(id INTEGER PRIMARY KEY CHECK(id=1),version INTEGER NOT NULL,next_generation INTEGER NOT NULL); INSERT OR IGNORE INTO object_set_meta VALUES(1,1,1); CREATE TABLE IF NOT EXISTS object_sets(pin TEXT PRIMARY KEY,receipt TEXT NOT NULL,refs TEXT NOT NULL,count INTEGER NOT NULL CHECK(count>=0));")?;
    let existing: i64 =
        connection.query_row("SELECT count(*) FROM object_sets", [], |row| row.get(0))?;
    ensure!(
        existing == 0,
        "v1 retention has unexpected explicit pins; refusing recovery"
    );
    schema(&connection)?;
    // No explicit pin is inserted before this fail-closed version is durable.
    state.format_version = 2;
    store.commit_state(state)?;
    Ok(())
}
fn schema(connection: &Connection) -> Result<()> {
    let (version, next): (i64, i64) = connection.query_row(
        "SELECT version,next_generation FROM object_set_meta WHERE id=1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    ensure!(
        version == 1 && next > 0,
        "unsupported explicit retention ledger"
    );
    Ok(())
}
fn normalize(objects: &[ObjectRef]) -> Result<(Vec<ObjectRef>, Vec<u8>, u64)> {
    ensure!(
        objects.len() <= MAX_OBJECTS,
        "explicit object set exceeds count limit"
    );
    let mut objects = objects.to_vec();
    objects.sort();
    objects.dedup();
    let mut total = 0u64;
    for object in &objects {
        object.validate()?;
        total = total
            .checked_add(object.size_bytes)
            .context("explicit retained byte count overflow")?;
        ensure!(
            total <= MAX_SET_BYTES,
            "explicit object set exceeds byte limit"
        );
    }
    let encoded = canonical_json(&objects)?;
    ensure!(
        encoded.len() <= MAX_SET_JSON,
        "explicit object set exceeds encoded limit"
    );
    Ok((objects, encoded, total))
}
fn decode(
    pin: &str,
    receipt: &str,
    objects: &str,
    count: i64,
) -> Result<(ObjectSetReceipt, Vec<ObjectRef>)> {
    ensure!(
        receipt.len() <= 16 * 1024 && objects.len() <= MAX_SET_JSON,
        "oversized explicit retention row"
    );
    let receipt: ObjectSetReceipt = serde_json::from_str(receipt)?;
    let parsed: Vec<ObjectRef> = serde_json::from_str(objects)?;
    let (normalized, encoded, total) = normalize(&parsed)?;
    ensure!(
        encoded == objects.as_bytes() && normalized == parsed,
        "noncanonical explicit retention row"
    );
    ensure!(
        receipt.format_version == 1
            && receipt.pin_id == pin
            && receipt.generation > 0
            && receipt.generation <= i64::MAX as u64
            && Uuid::parse_str(&receipt.staging_id)
                .is_ok_and(|id| id.to_string() == receipt.staging_id)
            && count >= 0
            && receipt.object_count == parsed.len() as u64
            && count as u64 == receipt.object_count
            && receipt.total_bytes == total
            && receipt.set_digest == ObjectRef::from_bytes(ObjectKind::Blob, &encoded).digest,
        "explicit retention receipt differs from its objects"
    );
    validate_pin(pin)?;
    Ok((receipt, parsed))
}
fn existing(
    connection: &Connection,
    pin: &str,
) -> Result<Option<(ObjectSetReceipt, Vec<ObjectRef>)>> {
    let row: Option<(String, String, i64)> = connection
        .query_row(
            "SELECT receipt,refs,count FROM object_sets WHERE pin=?",
            [pin],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    row.map(|(receipt, refs, count)| decode(pin, &receipt, &refs, count))
        .transpose()
}
fn verify_object(store: &LocalStore, reference: &ObjectRef, started: Instant) -> Result<()> {
    let check = || -> Result<()> {
        ensure!(
            started.elapsed() < VALIDATION_BUDGET,
            "explicit object validation budget exceeded"
        );
        Ok(())
    };
    check()?;
    if reference.kind == ObjectKind::Blob {
        store.verify_blob_stream_locked(reference, |_, _| check())?;
    } else {
        ensure!(
            reference.size_bytes <= 4 * 1024 * 1024,
            "explicit metadata needs bounded bulk retention"
        );
        store.read_locked(reference)?;
    }
    check()?;
    open_file(&store.object_path(reference)?, false, false)?.sync_all()?;
    Ok(())
}

impl LocalStore {
    /// Active individual-object retention only; this never resolves a root pin.
    pub fn object_set_receipt(&self, pin: &str) -> Result<Option<ObjectSetReceipt>> {
        validate_pin(pin)?;
        let _lock = self.lock()?;
        if self.read_state()?.format_version == 1 {
            return Ok(None);
        }
        let connection = connect(self)?;
        schema(&connection)?;
        Ok(existing(&connection, pin)?.map(|(receipt, _)| receipt))
    }
    /// Blocking bounded verification/fsync; call outside the engine/App actor.
    /// The staging lease remains live until the explicit set is durably stored.
    /// An identical active pin returns its original receipt on retry.
    pub fn retain_object_set(
        &self,
        staging_id: &str,
        pin: &str,
        objects: &[ObjectRef],
    ) -> Result<ObjectSetReceipt> {
        validate_pin(pin)?;
        let (objects, encoded, total) = normalize(objects)?;
        let set_digest = ObjectRef::from_bytes(ObjectKind::Blob, &encoded).digest;
        let _lock = self.lock()?;
        let mut state = self.read_state()?;
        if state.format_version == 2 {
            let connection = connect(self)?;
            schema(&connection)?;
            if let Some((receipt, _)) = existing(&connection, pin)? {
                ensure!(
                    receipt.set_digest == set_digest,
                    "explicit pin already names another object set"
                );
                ensure!(
                    receipt.staging_id == staging_id,
                    "explicit pin belongs to another staging operation"
                );
                if state.staging.remove(staging_id).is_some() {
                    self.commit_state(&mut state)?;
                }
                return Ok(receipt);
            }
        }
        let lease = state
            .staging
            .get(staging_id)
            .context("explicit retention requires a live staging lease")?;
        ensure!(
            lease.expires_at_ms > now_ms()?,
            "explicit retention staging expired"
        );
        let started = Instant::now();
        let mut shards = BTreeSet::new();
        for reference in &objects {
            verify_object(self, reference, started)?;
            shards.insert(self.object_path(reference)?.parent().unwrap().to_path_buf());
        }
        for shard in shards {
            sync_directory(&shard)?;
        }
        sync_directory(&self.root.join("objects"))?;
        ensure!(
            state.staging[staging_id].expires_at_ms > now_ms()?,
            "explicit retention staging expired during validation"
        );
        initialize(self, &mut state)?;
        let mut connection = connect(self)?;
        schema(&connection)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (records, references): (i64, i64) = transaction.query_row(
            "SELECT count(*),coalesce(sum(count),0) FROM object_sets",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        ensure!(
            records >= 0
                && records < MAX_RECORDS as i64
                && references >= 0
                && references
                    .checked_add(objects.len() as i64)
                    .is_some_and(|count| count <= MAX_TOTAL_REFERENCES as i64),
            "explicit retention ledger admission is full"
        );
        let generation: i64 = transaction.query_row(
            "SELECT next_generation FROM object_set_meta WHERE id=1",
            [],
            |row| row.get(0),
        )?;
        let next = generation
            .checked_add(1)
            .context("explicit retention generation exhausted")?;
        let receipt = ObjectSetReceipt {
            format_version: 1,
            pin_id: pin.into(),
            staging_id: staging_id.into(),
            set_digest,
            generation: generation.try_into()?,
            object_count: objects.len() as u64,
            total_bytes: total,
        };
        transaction.execute(
            "INSERT INTO object_sets VALUES(?,?,?,?)",
            params![
                pin,
                String::from_utf8(canonical_json(&receipt)?)?,
                String::from_utf8(encoded)?,
                objects.len() as i64
            ],
        )?;
        transaction.execute(
            "UPDATE object_set_meta SET next_generation=? WHERE id=1",
            [next],
        )?;
        ensure!(
            state.staging[staging_id].expires_at_ms > now_ms()?,
            "explicit retention staging expired before commit"
        );
        transaction.commit()?;
        sync_directory(&self.root)?;
        // Two authorities cannot be committed atomically. This order leaves
        // conservative staging on any interrupted/failed acknowledgment.
        state.staging.remove(staging_id);
        self.commit_state(&mut state)?;
        Ok(receipt)
    }
    pub fn object_set_refs(&self, receipt: &ObjectSetReceipt) -> Result<Vec<ObjectRef>> {
        let _lock = self.lock()?;
        ensure!(
            self.read_state()?.format_version == 2,
            "explicit retention ledger is not enabled"
        );
        let connection = connect(self)?;
        schema(&connection)?;
        let (current, objects) =
            existing(&connection, &receipt.pin_id)?.context("explicit pin is absent")?;
        ensure!(current == *receipt, "explicit pin receipt is stale");
        Ok(objects)
    }
    /// Compare the complete receipt, including its monotonic generation. A
    /// delayed caller cannot release a newer pin which reused the same name.
    pub fn release_object_set(&self, receipt: &ObjectSetReceipt) -> Result<()> {
        let _lock = self.lock()?;
        ensure!(
            self.read_state()?.format_version == 2,
            "explicit retention ledger is not enabled"
        );
        let mut connection = connect(self)?;
        schema(&connection)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some((current, _)) = existing(&transaction, &receipt.pin_id)? {
            ensure!(current == *receipt, "explicit pin receipt is stale");
            transaction.execute("DELETE FROM object_sets WHERE pin=?", [&receipt.pin_id])?;
        }
        transaction.commit()?;
        Ok(())
    }
}

pub(super) fn retained_for_gc(store: &LocalStore) -> Result<Vec<ObjectRef>> {
    let connection = connect(store)?;
    schema(&connection)?;
    let integrity: String = connection.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
    ensure!(
        integrity == "ok",
        "explicit retention ledger is corrupt; GC disabled"
    );
    let mut statement =
        connection.prepare("SELECT pin,receipt,refs,count FROM object_sets ORDER BY pin")?;
    let mut rows = statement.query([])?;
    let mut objects = BTreeSet::new();
    let mut records = 0usize;
    while let Some(row) = rows.next()? {
        records += 1;
        ensure!(
            records <= MAX_RECORDS,
            "explicit retention record limit exceeded"
        );
        let pin: String = row.get(0)?;
        let receipt: String = row.get(1)?;
        let refs: String = row.get(2)?;
        let count: i64 = row.get(3)?;
        let (_, references) = decode(&pin, &receipt, &refs, count)?;
        objects.extend(references);
        ensure!(
            objects.len() <= MAX_TOTAL_REFERENCES,
            "explicit retained object limit exceeded"
        );
    }
    // Validate listed objects only. A missing remote child is intentional;
    // schema/hash failure of a listed local object aborts before any sweep.
    let started = Instant::now();
    for reference in &objects {
        verify_object(store, reference, started)?;
    }
    Ok(objects.into_iter().collect())
}
