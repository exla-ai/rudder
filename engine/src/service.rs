//! Versioned, transport-independent immutable workspace operations. These are
//! trusted local engine operations, not the run-scoped agent tool API.

use crate::paths::FabricPaths;
use crate::protocol::{CheckpointConsistency, Durability, ObjectKind, ObjectRef, FORMAT_VERSION};
use crate::store::{
    CheckpointManifest, DirectoryEntry, EnvironmentManifest, ImportPolicy, LocalStore,
    SourceManifest,
};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::Duration;
use uuid::Uuid;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoreRequest {
    pub format_version: u32,
    pub request: StoreOperation,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum StoreOperation {
    Capabilities,
    ImportSource {
        path: PathBuf,
        /// Caller assertion backed by an engine barrier, never inferred from a
        /// timestamp or the absence of recent PTY output.
        writers_quiesced: bool,
        #[serde(default)]
        excluded_paths: Vec<Vec<Vec<u8>>>,
        #[serde(default)]
        parents: Vec<ObjectRef>,
        pin: String,
    },
    PrepareEnvironment {
        path: PathBuf,
        writers_quiesced: bool,
        platform: BTreeMap<String, String>,
        installer_inputs: Vec<ObjectRef>,
        provenance: BTreeMap<String, String>,
        pin: String,
    },
    UpdateSource {
        source: ObjectRef,
        path: Vec<Vec<u8>>,
        replacement: Option<DirectoryEntry>,
        pin: String,
    },
    Checkpoint {
        source: ObjectRef,
        environment: Option<ObjectRef>,
        #[serde(default)]
        parents: Vec<ObjectRef>,
        knowledge_cursor: u64,
        pin: String,
    },
    ExportTree {
        directory: ObjectRef,
        destination: PathBuf,
    },
    Read {
        object: ObjectRef,
    },
    Inventory {
        root: ObjectRef,
    },
    PutBlob {
        bytes_hex: String,
    },
    Receive {
        object: ObjectRef,
        bytes_hex: String,
    },
    Pin {
        pin: String,
        root: ObjectRef,
        #[serde(default)]
        existing: bool,
    },
    Unpin {
        pin: String,
    },
    Retention,
    BeginStaging {
        ttl_ms: u64,
    },
    RenewStaging {
        lease_id: String,
        ttl_ms: u64,
    },
    AbandonStaging {
        lease_id: String,
    },
    Publish {
        lease_id: String,
        pin: String,
        root: ObjectRef,
    },
    Gc {
        grace_ms: u64,
    },
    /// Reference transport between independent local-volume stores. This does
    /// not claim object-store replication or survive loss of both volumes.
    TransferLocal {
        root: ObjectRef,
        destination_store: PathBuf,
        pin: String,
    },
}

pub fn handle_store(paths: &FabricPaths, request: StoreRequest) -> Result<Value> {
    let store = LocalStore::open(&paths.cas)?;
    handle_store_on(&store, request)
}

pub fn handle_store_on(store: &LocalStore, request: StoreRequest) -> Result<Value> {
    ensure!(
        request.format_version == FORMAT_VERSION,
        "unsupported fabric request version"
    );
    let result = match request.request {
        StoreOperation::Capabilities => json!({
            "object_hash":"sha256", "metadata_encoding":"canonical-json-v1",
            "authority":"local-only", "checkpoint_consistency":["quiesced"],
            "checkpoint_durability":["local"], "metadata_persistent_cow":true,
            "materializers":["eager-private-export"], "lazy_materialization":false,
            "cas_formats":{
                "object_kinds":["blob","file","directory","directory_shard","source_root","environment_root","checkpoint","runnable_root","action_key","action_result"],
                "source_versions":[1,2],"environment_versions":[1,2],"runnable_root_versions":[1],
                "directory_versions":[1,2],"file_layout":crate::content::FILE_LAYOUT,
                "file_chunk_bytes":crate::content::CHUNK_BYTES,
                "content_descriptor_max_bytes":crate::content::MAX_FILE_NODE_BYTES,
            },
        }),
        StoreOperation::ImportSource {
            path,
            writers_quiesced,
            excluded_paths,
            parents,
            pin,
        } => {
            ensure!(
                writers_quiesced,
                "source import requires an engine writer barrier"
            );
            let root = publish_built(&store, &pin, || {
                let directory = store.import_tree_with_policy(
                    &path,
                    CheckpointConsistency::Quiesced,
                    &ImportPolicy { excluded_paths },
                )?;
                store.put_source(&SourceManifest {
                    format_version: FORMAT_VERSION,
                    directory,
                    parents,
                    consistency: CheckpointConsistency::Quiesced,
                })
            })?;
            json!({"root":root,"pin":pin,"durability":"local"})
        }
        StoreOperation::PrepareEnvironment {
            path,
            writers_quiesced,
            platform,
            installer_inputs,
            provenance,
            pin,
        } => {
            ensure!(
                writers_quiesced,
                "environment preparation requires quiesced writers"
            );
            ensure!(
                !platform.is_empty(),
                "environment platform and ABI must be declared"
            );
            let root = publish_built(&store, &pin, || {
                let directory = store.import_tree(path, CheckpointConsistency::Quiesced)?;
                store.put_environment(&EnvironmentManifest {
                    format_version: FORMAT_VERSION,
                    directory,
                    platform,
                    installer_inputs,
                    provenance,
                })
            })?;
            json!({"root":root,"pin":pin,"durability":"local"})
        }
        StoreOperation::UpdateSource {
            source,
            path,
            replacement,
            pin,
        } => {
            ensure!(
                source.kind == ObjectKind::SourceRoot,
                "expected source root"
            );
            let source_manifest: SourceManifest =
                serde_json::from_slice(&store.read_object(&source)?)?;
            let root = publish_built(&store, &pin, || {
                let directory =
                    store.update_path(&source_manifest.directory, &path, replacement)?;
                store.put_source(&SourceManifest {
                    format_version: FORMAT_VERSION,
                    directory,
                    parents: vec![source],
                    consistency: CheckpointConsistency::Quiesced,
                })
            })?;
            json!({"root":root,"pin":pin,"durability":"local"})
        }
        StoreOperation::Checkpoint {
            source,
            environment,
            parents,
            knowledge_cursor,
            pin,
        } => {
            let root = publish_built(&store, &pin, || {
                store.put_checkpoint(&CheckpointManifest {
                    format_version: FORMAT_VERSION,
                    source,
                    environment,
                    parents,
                    consistency: CheckpointConsistency::Quiesced,
                    durability: Durability::Local,
                    knowledge_cursor,
                })
            })?;
            json!({"root":root,"pin":pin,"durability":"local"})
        }
        StoreOperation::ExportTree {
            directory,
            destination,
        } => {
            store.export_tree(&directory, &destination)?;
            json!({"path":destination,"materialization":"eager-private-export"})
        }
        StoreOperation::Read { object } => {
            let bytes = store.read_object(&object)?;
            if object.kind == ObjectKind::Blob {
                json!({"object":object,"bytes_hex":hex::encode(bytes)})
            } else {
                json!({"object":object,"metadata":serde_json::from_slice::<Value>(&bytes)?})
            }
        }
        StoreOperation::Inventory { root } => json!({"objects":store.closure(&root)?}),
        StoreOperation::PutBlob { bytes_hex } => {
            json!({"object":store.put_blob(&hex::decode(bytes_hex)?)?})
        }
        StoreOperation::Receive { object, bytes_hex } => {
            store.receive(&object, &hex::decode(bytes_hex)?)?;
            json!({"object":object})
        }
        StoreOperation::Pin {
            pin,
            root,
            existing,
        } => {
            let generation = if existing {
                store.pin_existing(&pin, &root)?
            } else {
                store.pin(&pin, &root)?
            };
            json!({"generation":generation})
        }
        StoreOperation::Unpin { pin } => {
            store.unpin(&pin)?;
            json!({"released":true})
        }
        StoreOperation::Retention => serde_json::to_value(store.retention()?)?,
        StoreOperation::BeginStaging { ttl_ms } => {
            serde_json::to_value(store.begin_staging(lease_ttl(ttl_ms)?)?)?
        }
        StoreOperation::RenewStaging { lease_id, ttl_ms } => {
            serde_json::to_value(store.renew_staging(&lease_id, lease_ttl(ttl_ms)?)?)?
        }
        StoreOperation::AbandonStaging { lease_id } => {
            store.abandon_staging(&lease_id)?;
            json!({"abandoned":true})
        }
        StoreOperation::Publish {
            lease_id,
            pin,
            root,
        } => json!({"generation":store.publish(&lease_id, &pin, &root)?}),
        StoreOperation::Gc { grace_ms } => {
            serde_json::to_value(store.gc(Duration::from_millis(grace_ms))?)?
        }
        StoreOperation::TransferLocal {
            root,
            destination_store,
            pin,
        } => {
            let target = LocalStore::open(destination_store)?;
            transfer_local(&store, &target, &root, &pin)?
        }
    };
    Ok(json!({"format_version":FORMAT_VERSION,"result":result}))
}

pub const TRANSFER_MAX_OBJECT_BYTES: u64 = 256 * 1024 * 1024;
pub const TRANSFER_MAX_METADATA_BYTES: u64 = 4 * 1024 * 1024;
const TRANSFER_MAX_OBJECTS: usize = 100_000;
const TRANSFER_MAX_BYTES: u64 = 4 * 1024 * 1024 * 1024;

struct TransferPin {
    root: ObjectRef,
    inventory: BTreeSet<ObjectRef>,
}
struct PartialTransferPin {
    staging_id: String,
    expected: Vec<ObjectRef>,
    receipt: Option<crate::store::ObjectSetReceipt>,
    inventory: BTreeSet<ObjectRef>,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartialTransferInventory {
    pub format_version: u32,
    pub owned_pin: String,
    pub plan: ObjectRef,
    pub directory: ObjectRef,
    pub hardlinks: ObjectRef,
    pub base: crate::remote_root::RootBinding,
    pub objects: Vec<ObjectRef>,
}

/// An exact transfer-owned DATA clone. This inventory is neither a complete
/// local root pin nor an execution/accepted-history receipt. Receipt counters
/// stay in their opaque CAS documents rather than crossing JavaScript numbers.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartialCheckpointTransferInventory {
    pub format_version: u32,
    pub owned_pin: String,
    pub publication_id: String,
    pub publication: ObjectRef,
    pub q: ObjectRef,
    pub source: ObjectRef,
    pub runnable: ObjectRef,
    pub plan: ObjectRef,
    pub freeze_intent: ObjectRef,
    pub drain_record: ObjectRef,
    pub base: crate::remote_root::RootBinding,
    pub objects: Vec<ObjectRef>,
}

/// One trusted local transport session. Resources allocated here are distinct
/// from caller pins/leases and only this session's resources are cleaned up.
/// A hard process kill can leave a conservative pin; it never acknowledges a
/// remote publication or silently removes an unrelated retention authority.
pub struct TransferSession {
    store: LocalStore,
    source_pins: BTreeMap<String, TransferPin>,
    partial_pins: BTreeMap<String, PartialTransferPin>,
    staging: BTreeSet<String>,
}
impl TransferSession {
    pub fn open(paths: &FabricPaths) -> Result<Self> {
        Ok(Self {
            store: LocalStore::open(&paths.cas)?,
            source_pins: BTreeMap::new(),
            partial_pins: BTreeMap::new(),
            staging: BTreeSet::new(),
        })
    }
    pub fn call(&mut self, request: StoreRequest) -> Result<Value> {
        let finished = match &request.request {
            StoreOperation::RenewStaging { lease_id, .. }
            | StoreOperation::AbandonStaging { lease_id }
            | StoreOperation::Publish { lease_id, .. } => {
                ensure!(
                    self.staging.contains(lease_id),
                    "lease is outside this transfer session"
                );
                if matches!(&request.request, StoreOperation::RenewStaging { .. }) {
                    None
                } else {
                    Some(lease_id.clone())
                }
            }
            _ => None,
        };
        ensure!(
            !matches!(&request.request, StoreOperation::BeginStaging { .. })
                || self.staging.len() < 32,
            "too many session staging leases"
        );
        let begin = matches!(&request.request, StoreOperation::BeginStaging { .. });
        let response = handle_store_on(&self.store, request)?;
        if begin {
            self.staging.insert(
                response["result"]["id"]
                    .as_str()
                    .context("missing local staging identity")?
                    .to_owned(),
            );
        }
        if let Some(id) = finished {
            self.staging.remove(&id);
        }
        Ok(response["result"].clone())
    }
    pub fn pin_source(&mut self, root: &ObjectRef) -> Result<String> {
        transfer_reference(root)?;
        ensure!(
            self.source_pins.len() + self.partial_pins.len() < 32,
            "too many session source pins"
        );
        let pin = format!("transfer:rpc:{}", Uuid::new_v4());
        // A direct retained root already has verification authority. A root
        // found only as a descendant must receive a fresh complete check.
        if self
            .store
            .retention()?
            .pins
            .values()
            .any(|value| value == root)
        {
            self.store.pin_existing(&pin, root)?;
        } else {
            self.store.pin(&pin, root)?;
        }
        self.source_pins.insert(
            pin.clone(),
            TransferPin {
                root: root.clone(),
                inventory: BTreeSet::new(),
            },
        );
        Ok(pin)
    }
    pub fn unpin_source(&mut self, pin: &str) -> Result<()> {
        if let Some(partial) = self.partial_pins.get(pin) {
            // Original stage+set cannot identify the first generation after
            // a crash-stage was released/reused with the same UUID. Looking
            // up a current name would turn unknown DATA into cleanup authority.
            // Preserve this entry and any existing original GC stage instead;
            // consumed stages are never revived by finish/Drop.
            let receipt = partial.receipt.as_ref().context(
                "transfer ACK generation is unknown; retention cleanup remains unconfirmed",
            )?;
            self.store.release_object_set(receipt)?;
            self.partial_pins.remove(pin);
            return Ok(());
        }
        ensure!(
            self.source_pins.contains_key(pin),
            "pin is outside this transfer session"
        );
        self.store.unpin(pin)?;
        self.source_pins.remove(pin);
        Ok(())
    }
    /// Transfer retention never turns a partial tree into a complete local pin.
    /// The data plan/receipt is validated before copying its exact local units.
    pub fn pin_partial_source(
        &mut self,
        record: &crate::delta::PartialTreeReceipt,
    ) -> Result<PartialTransferInventory> {
        ensure!(
            self.source_pins.len() + self.partial_pins.len() < 32,
            "too many session source pins"
        );
        ensure!(self.staging.len() < 32, "too many session staging leases");
        let lease = self.store.begin_staging(Duration::from_secs(600))?;
        self.staging.insert(lease.id.clone());
        let mut owned_pin = None;
        let result = (|| {
            // The lease fences GC even if the producer retires its original pin
            // between reading the record and committing transfer retention.
            let plan = crate::delta::read_partial_tree(&self.store, record)?;
            let objects = self.store.object_set_refs(&record.local)?;
            ensure!(
                objects.len() <= TRANSFER_MAX_OBJECTS,
                "partial inventory exceeds object limit"
            );
            let mut total = 0u64;
            for object in &objects {
                transfer_reference(object)?;
                total = total
                    .checked_add(object.size_bytes)
                    .context("partial inventory size overflow")?;
            }
            ensure!(
                total <= TRANSFER_MAX_BYTES,
                "partial inventory exceeds byte limit"
            );
            let pin = format!("transfer:partial:rpc:{}", Uuid::new_v4());
            owned_pin = Some(pin.clone());
            self.clone_partial_pin(&lease.id, &pin, &objects)?;
            Ok(PartialTransferInventory {
                format_version: 1,
                owned_pin: pin,
                plan: record.plan.clone(),
                directory: plan.directory,
                hardlinks: plan.hardlinks,
                base: plan.base,
                objects,
            })
        })();
        if result.is_err() {
            self.clean_failed_partial(&lease.id, owned_pin.as_deref());
        }
        result
    }

    /// Clone all explicit capture/evidence/wrapper units before exposing any
    /// producer reads to the caller. Staging begins before the first producer
    /// receipt/CAS read. No inherited payload or accepted seed is hydrated.
    pub fn pin_partial_checkpoint(
        &mut self,
        record: &crate::partial_checkpoint::PartialCheckpointDataReceipt,
    ) -> Result<PartialCheckpointTransferInventory> {
        ensure!(
            self.source_pins.len() + self.partial_pins.len() < 32,
            "too many session source pins"
        );
        ensure!(self.staging.len() < 32, "too many session staging leases");
        let lease = self
            .store
            .begin_staging(crate::partial_checkpoint::STAGING_TTL)?;
        self.staging.insert(lease.id.clone());
        let mut owned_pin = None;
        let result = (|| {
            let data =
                crate::partial_checkpoint::read_partial_checkpoint_data(&self.store, record)?;
            let objects = data.objects().to_vec();
            let payload = &data.publication().payload;
            let pin = format!("transfer:partial-checkpoint:rpc:{}", Uuid::new_v4());
            owned_pin = Some(pin.clone());
            self.clone_partial_pin(&lease.id, &pin, &objects)?;
            // Original producer receipts may legitimately retire after their
            // validation. The acknowledged clone now owns these exact bytes;
            // do not require the old pins to remain active after this point.
            Ok(PartialCheckpointTransferInventory {
                format_version: 1,
                owned_pin: pin,
                publication_id: payload.publication_id.clone(),
                publication: record.publication.clone(),
                q: payload.q.clone(),
                source: payload.source.clone(),
                runnable: payload.runnable.clone(),
                plan: payload.plan.clone(),
                freeze_intent: payload.freeze_intent.clone(),
                drain_record: payload.drain_record.clone(),
                base: data.q().capture.base.clone(),
                objects,
            })
        })();
        if result.is_err() {
            self.clean_failed_partial(&lease.id, owned_pin.as_deref());
        }
        result
    }

    fn clone_partial_pin(
        &mut self,
        staging_id: &str,
        pin: &str,
        objects: &[ObjectRef],
    ) -> Result<()> {
        let expected = crate::partial_checkpoint::bounded_objects(objects)?;
        ensure!(expected == objects, "noncanonical partial clone inventory");
        self.partial_pins.insert(
            pin.into(),
            PartialTransferPin {
                staging_id: staging_id.into(),
                expected: expected.clone(),
                receipt: None,
                inventory: BTreeSet::new(),
            },
        );
        let receipt = self.store.retain_object_set(staging_id, pin, objects)?;
        // Save the complete returned receipt before later checks can fail, so
        // cleanup always compares this generation rather than a current name.
        self.partial_pins.get_mut(pin).unwrap().receipt = Some(receipt.clone());
        crate::partial_checkpoint::check_receipt_set(
            &receipt,
            staging_id,
            &self.partial_pins[pin].expected,
        )?;
        ensure!(
            receipt.pin_id == pin && self.store.object_set_refs(&receipt)? == expected,
            "partial clone ACK differs from its exact retained inventory"
        );
        self.staging.remove(staging_id);
        self.partial_pins.get_mut(pin).unwrap().inventory = expected.into_iter().collect();
        Ok(())
    }

    fn clean_failed_partial(&mut self, staging_id: &str, pin: Option<&str>) {
        if pin.is_some_and(|pin| {
            self.partial_pins.contains_key(pin) && self.unpin_source(pin).is_err()
        }) {
            // Leave uncertain retention and its original staging discoverable
            // for retry/recovery. Never erase the only conservative GC fence.
            return;
        }
        if self.store.abandon_staging(staging_id).is_ok() {
            self.staging.remove(staging_id);
        }
    }
    pub fn inventory(&mut self, root: &ObjectRef) -> Result<Vec<ObjectRef>> {
        ensure!(
            self.source_pins.values().any(|pin| pin.root == *root),
            "source root is not retained by this session"
        );
        let objects = self.store.retained_closure(root)?;
        ensure!(
            objects.len() <= TRANSFER_MAX_OBJECTS,
            "source inventory exceeds object limit"
        );
        let mut total = 0_u64;
        for object in &objects {
            transfer_reference(object)?;
            total = total
                .checked_add(object.size_bytes)
                .context("source inventory size overflow")?;
        }
        ensure!(
            total <= TRANSFER_MAX_BYTES,
            "source inventory exceeds byte limit"
        );
        let inventory: BTreeSet<ObjectRef> = objects.iter().cloned().collect();
        for pin in self
            .source_pins
            .values_mut()
            .filter(|pin| pin.root == *root)
        {
            pin.inventory = inventory.clone();
        }
        Ok(objects)
    }
    pub fn read(&self, object: &ObjectRef) -> Result<Vec<u8>> {
        transfer_reference(object)?;
        ensure!(
            self.source_pins
                .values()
                .any(|pin| pin.inventory.contains(object))
                || self
                    .partial_pins
                    .values()
                    .any(|pin| pin.inventory.contains(object)),
            "object is outside retained source inventory"
        );
        self.store.read_object(object)
    }
    pub fn receive(
        &mut self,
        lease_id: &str,
        object: &ObjectRef,
        bytes: &[u8],
    ) -> Result<Vec<ObjectRef>> {
        transfer_reference(object)?;
        ensure!(
            self.staging.contains(lease_id),
            "lease is outside this transfer session"
        );
        self.store.receive_staged(lease_id, object, bytes)?;
        // Blob content is opaque. Only bounded metadata is decoded for edges.
        if object.kind == ObjectKind::Blob {
            Ok(Vec::new())
        } else {
            self.store.references(object)
        }
    }

    /// Explicit EOF acknowledgment. Drop is only a crash/unwind fallback and
    /// cannot communicate cleanup failure to the transport's caller.
    pub fn finish(&mut self) -> Result<()> {
        let mut failed = false;
        for pin in self.source_pins.keys().cloned().collect::<Vec<_>>() {
            if self.store.unpin(&pin).is_ok() {
                self.source_pins.remove(&pin);
            } else {
                failed = true;
            }
        }
        for pin in self.partial_pins.keys().cloned().collect::<Vec<_>>() {
            if self.unpin_source(&pin).is_err() {
                failed = true;
            }
        }
        for lease in self.staging.iter().cloned().collect::<Vec<_>>() {
            if self
                .partial_pins
                .values()
                .any(|pin| pin.staging_id == lease)
            {
                failed = true;
                continue;
            }
            if self.store.abandon_staging(&lease).is_ok() {
                self.staging.remove(&lease);
            } else {
                failed = true;
            }
        }
        ensure!(
            !failed,
            "transfer-owned retention cleanup was not confirmed"
        );
        Ok(())
    }
}
impl Drop for TransferSession {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

#[cfg(test)]
#[path = "partial_checkpoint/session_tests.rs"]
mod partial_checkpoint_tests;

pub fn transfer_reference(reference: &ObjectRef) -> Result<()> {
    reference.validate()?;
    ensure!(
        reference.size_bytes <= TRANSFER_MAX_OBJECT_BYTES,
        "transfer object exceeds byte limit"
    );
    ensure!(
        reference.kind == ObjectKind::Blob || reference.size_bytes <= TRANSFER_MAX_METADATA_BYTES,
        "transfer metadata exceeds byte limit"
    );
    ensure!(
        !matches!(
            reference.kind,
            ObjectKind::File | ObjectKind::DirectoryShard | ObjectKind::RunnableRoot
        ) || reference.size_bytes <= crate::content::MAX_FILE_NODE_BYTES as u64,
        "transfer content descriptor exceeds format byte limit"
    );
    Ok(())
}

fn lease_ttl(ttl_ms: u64) -> Result<Duration> {
    ensure!(
        (1_000..=3_600_000).contains(&ttl_ms),
        "staging TTL must be between one second and one hour"
    );
    Ok(Duration::from_millis(ttl_ms))
}

fn publish_built(
    store: &LocalStore,
    pin: &str,
    build: impl FnOnce() -> Result<ObjectRef>,
) -> Result<ObjectRef> {
    let lease = store.begin_staging(Duration::from_secs(3600))?;
    let result = (|| {
        let root = build()?;
        store.publish(&lease.id, pin, &root)?;
        Ok(root)
    })();
    if result.is_err() {
        let _ = store.abandon_staging(&lease.id);
    }
    result
}

pub fn transfer_local(
    source: &LocalStore,
    target: &LocalStore,
    root: &ObjectRef,
    pin: &str,
) -> Result<Value> {
    ensure!(
        source.root() != target.root(),
        "transfer requires independent stores"
    );
    // Retain the source for the entire inventory/read sequence. This exact pin
    // belongs to the transfer and is the only pin released by its cleanup.
    let source_pin = format!("transfer:{}", Uuid::new_v4());
    source.pin(&source_pin, root)?;
    let result: Result<Value> = (|| {
        let lease = target.begin_staging(Duration::from_secs(3600))?;
        let result: Result<Value> = (|| {
            let objects = source.closure(root)?;
            let mut transferred_objects = 0;
            let mut transferred_bytes = 0_u64;
            for object in &objects {
                if target.object_path(object)?.try_exists()? {
                    // Existing bytes are verified, never assumed correct by name.
                    target
                        .read_object(object)
                        .context("verify existing target object")?;
                    continue;
                }
                let bytes = source.read_object(object)?;
                target.receive(object, &bytes)?;
                transferred_objects += 1;
                transferred_bytes += bytes.len() as u64;
            }
            let generation = target.publish(&lease.id, pin, root)?;
            Ok(
                json!({"root":root,"generation":generation,"verified_objects":objects.len(),"transferred_objects":transferred_objects,"transferred_bytes":transferred_bytes,"durability":"local-mirror","transport":"local-store-reference"}),
            )
        })();
        if result.is_err() {
            let _ = target.abandon_staging(&lease.id);
        }
        result
    })();
    // A cleanup failure is reported, since leaking a retention pin matters to
    // operators even after successful publication.
    let cleanup = source.unpin(&source_pin);
    let result = result?;
    cleanup?;
    Ok(result)
}
