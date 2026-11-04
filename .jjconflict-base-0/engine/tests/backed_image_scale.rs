//! Explicit storage component qualification against an existing immutable image.
//! This does not grant Root, compiler, scheduler, task or retirement authority.
#![cfg(target_os = "linux")]

use anyhow::{ensure, Context, Result};
use rudder_engine::{
    build::{BuildIndex, PlatformIdentity, PreparationSpec, PreparedContentLayout, ToolchainIdentity},
    execution::finalization_budget::FinalizationBudget,
    protocol::ObjectRef,
    store::{
        backed_inputs::VerifiedBakedInputs, baked_image::BakedImageCas,
        ImageImportFence, LocalStore,
    },
};
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet}, fs::{self, OpenOptions},
    os::{fd::OwnedFd, unix::fs::OpenOptionsExt}, path::{Path, PathBuf},
    sync::{atomic::AtomicBool, Arc}, time::{Duration, Instant},
};

struct Clock(FinalizationBudget);
impl Clock {
    fn new() -> Self { Self(FinalizationBudget::begin(Duration::from_secs(30)).unwrap()) }
}
impl ImageImportFence for Clock {
    fn remaining(&self) -> Result<Duration> { self.0.remaining() }
}
fn descriptor(path: &Path, directory: bool) -> Result<OwnedFd> {
    Ok(OpenOptions::new().read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK
            | if directory { libc::O_DIRECTORY } else { 0 })
        .open(path)?.into())
}
fn object_count(root: &Path) -> Result<(usize, u64)> {
    let mut count = 0; let mut bytes = 0;
    for shard in fs::read_dir(root.join("objects"))? {
        let shard = shard?; ensure!(shard.file_type()?.is_dir(), "object shard differs");
        for file in fs::read_dir(shard.path())? {
            let file = file?; ensure!(file.file_type()?.is_file(), "object is not regular");
            count += 1; bytes += file.metadata()?.len();
        }
    }
    Ok((count, bytes))
}

#[test]
#[ignore = "requires an explicitly supplied immutable baked image; storage DATA only"]
fn actual_baked_image_preparation_gc_and_reattachment() -> Result<()> {
    let baked_root = PathBuf::from(std::env::var("RUDDER_BACKED_IMAGE_ROOT")?);
    let expected: ObjectRef = serde_json::from_str(&std::env::var("RUDDER_BACKED_IMAGE_INVENTORY")?)?;
    // The caller supplies a fresh empty owned directory on a large filesystem.
    let output = PathBuf::from(std::env::var("RUDDER_BACKED_IMAGE_OUTPUT")?);
    ensure!(output.is_absolute() && !output.exists(), "qualification output must be new");
    fs::create_dir(&output)?;
    let mut timings = BTreeMap::new();
    let start = Instant::now();
    let clock = Arc::new(Clock::new());
    let mut baked = None;
    BakedImageCas::capture_retaining(&mut baked, descriptor(&baked_root, true)?,
        descriptor(&baked_root.join("inventory.json"), false)?, expected.clone(), clock.clone())?;
    let baked = baked.context("actual baked image owner absent")?;
    let inventory = baked.inventory_data()?;
    let mut inputs = None;
    VerifiedBakedInputs::capture_retaining(&mut inputs, baked.clone(), clock.clone())?;
    let mut manifest_read = None;
    baked.read_manifest_retaining(&mut manifest_read, clock.clone())?;
    let manifest: serde_json::Value = serde_json::from_slice(
        manifest_read.as_ref().unwrap().verified_bytes()?)?;
    let native = manifest["files"].as_array().context("complete manifest files absent")?
        .iter().find(|f| f["path"] == "/opt/rudder/dist/native/rudder-native")
        .context("actual runtime native file absent from manifest")?;
    let executable: ObjectRef = serde_json::from_value(native["content"].clone())?;
    let executable_sha = native["sha256"].as_str().context("runtime native hash absent")?;
    clock.remaining()?;
    timings.insert("capture_typed_inputs_ms", start.elapsed().as_secs_f64() * 1000.0);
    let inputs = inputs.context("actual typed backing absent")?;
    ensure!(inputs.binding_data()?.object_count >= 1000, "fixture is below qualification scale");
    let local = LocalStore::open(output.join("cas"))?;
    let index = Arc::new(BuildIndex::open(output.join("index"), local.clone())?);
    // The identity describes actual manifest content; this storage DATA test
    // issues no execution or compiler SDK authority.
    let spec = PreparationSpec {
        format_version: 1, base_runtime: inventory.roots.installer_input.clone(),
        platform: PlatformIdentity { os: "linux".into(), architecture: std::env::consts::ARCH.into(),
            abi: "storage-component-only".into(), properties: BTreeMap::new() },
        installer_inputs: BTreeMap::from([
            ("image/tree".into(), inventory.roots.installer_input.clone()),
            ("image/manifest".into(), inventory.roots.manifest.clone()),
        ]),
        installer_commands: Vec::new(), installer_environment: BTreeMap::new(),
        toolchains: BTreeMap::from([("runtime-native-data".into(), ToolchainIdentity {
            version: format!("sha256:{executable_sha}"), closure: inventory.roots.installer_input.clone(),
            executable, executable_path: "package/dist/native/rudder-native".into(),
            configuration: BTreeMap::from([("role".into(), "storage-component-data".into())]),
        })]),
        policy_version: "backed-scale-data-v1".into(), effects: BTreeSet::new(),
    };
    let start = Instant::now(); let original = Clock::new(); let constraint = original.0.constraint();
    let mut cold = None;
    index.prepare_backed_retaining(&mut cold, spec.clone(), PreparedContentLayout::PagedChunkedV2,
        inventory.roots.installer_input.clone(), Vec::new(), vec![inputs.clone()], None,
        original.0.clone(), Arc::new(AtomicBool::new(false)))?;
    original.remaining()?;
    let cold = cold.context("original cold preparation absent")?;
    ensure!(cold.original_constraint_data() == constraint, "cold preparation renewed original clock");
    let ack = cold.acknowledgment()?; ensure!(!ack.reused(), "fresh preparation unexpectedly reused");
    timings.insert("cold_preparation_ms", start.elapsed().as_secs_f64() * 1000.0);

    let start = Instant::now(); let original = Clock::new(); let constraint = original.0.constraint();
    let mut warm = None;
    index.prepare_backed_retaining(&mut warm, spec, PreparedContentLayout::PagedChunkedV2,
        inventory.roots.installer_input.clone(), Vec::new(), vec![inputs.clone()], Some(ack),
        original.0.clone(), Arc::new(AtomicBool::new(false)))?;
    original.remaining()?;
    let warm = warm.context("original warm preparation absent")?;
    ensure!(warm.original_constraint_data() == constraint && warm.acknowledgment()?.reused(),
        "warm preparation did not use genuine prior ACK and original clock");
    timings.insert("warm_preparation_ms", start.elapsed().as_secs_f64() * 1000.0);
    ensure!(warm.acknowledgment()?.prepared() == ack.prepared(), "warm prepared roots differ");
    eprintln!("RUDDER_BACKED_IMAGE_SCALE_PARTIAL_V1={}", json!({"timings":timings,
        "scope":"storage-data-component-only", "nextOperation":"explicit-reattachment",
        "setupIncludedInColdPreparation":false, "coldMedia":false}));

    let state = local.retention()?;
    let pins = &state.backed_retention.as_ref().context("backed retention absent")?.pins;
    ensure!(pins.len() == 1, "unexpected backed pin count");
    let pin = pins.keys().next().unwrap().clone();
    let start = Instant::now(); let original = Arc::new(Clock::new()); let mut attached = None;
    let reopened = LocalStore::open(local.root())?;
    if let Err(error) = reopened.reattach_backed_retaining(&mut attached, pin, vec![inputs.clone()], original.clone()) {
        let detail = attached.as_ref().and_then(|owner| owner.failure())
            .map(|failure| format!("{failure:#}")).unwrap_or_else(|| "original carrier absent".into());
        eprintln!("RUDDER_BACKED_IMAGE_SCALE_FAILURE_V1={}", json!({"operation":"explicit-reattachment",
            "elapsedMs":start.elapsed().as_secs_f64()*1000.0,"retainedFailure":detail}));
        return Err(error.context(detail));
    }
    original.remaining()?;
    let attached = attached.context("original reattachment absent")?;
    let retained = attached.reattached_owner()?.clone();
    ensure!(retained.root_data() == &ack.prepared().receipt, "reattached pin differs");
    timings.insert("explicit_reattachment_ms", start.elapsed().as_secs_f64() * 1000.0);

    let start = Instant::now(); let original = Arc::new(Clock::new()); let mut gc = None;
    reopened.gc_backed_retaining(&mut gc, vec![retained], Duration::ZERO, original.clone())?;
    original.remaining()?;
    let gc = gc.context("original backed GC absent")?;
    let report = gc.acknowledgment()?;
    ensure!(!report.blocked_by_staging && report.removed_objects == 0, "GC skipped or lost retained objects");
    timings.insert("gc_ms", start.elapsed().as_secs_f64() * 1000.0);
    let (local_objects, local_bytes) = object_count(local.root())?;
    ensure!(local_bytes < 16 * 1024 * 1024, "image payload was copied into local CAS");
    for reference in inputs.objects_data()? {
        ensure!(!local.object_path(reference)?.exists(), "foreign image object copied locally");
    }
    let result = json!({"formatVersion":1, "scope":"storage-data-component-only",
        "inventory":expected, "binding":inputs.binding_data()?, "timings":timings,
        "perOperationOriginalDeadlineMs":30000, "localObjects":local_objects,
        "localEncodedBytes":local_bytes, "gcRemovedObjects":report.removed_objects,
        "sameLiveTypedBackingReattached":true, "preparationRestartRecovery":false,
        "compilerOrRootAuthority":false, "actualAgentLaunch":false});
    fs::write(output.join("result.json"), serde_json::to_vec_pretty(&result)?)?;
    println!("RUDDER_BACKED_IMAGE_SCALE_V1={result}");
    // Explicit disposable test process owns the remaining handles. This test
    // does not certify normal runtime retirement or remove persistent holds.
    Ok(())
}
