//! Actual accepted-corpus logical CoW COMPONENT, not Root/FUSE/agent capacity.
//! One retained parent and environment, fifty private completed checkpoints,
//! plus two retained negative attempts. No Journal rows or authority fixtures.
use super::Clock;
use anyhow::{ensure, Context, Result};
use jj_lib::object_id::ObjectId as _;
use rudder_engine::{
    directory::{EntryNode, LocalDirectoryStore, PagedDirectory},
    execution::finalization_budget::FinalizationBudget,
    protocol::{CheckpointConsistency, ObjectKind, ObjectRef, OwnerEpoch},
    snapshot::{self, RunnableRoot, SourceV2},
    store::{backed_inputs::VerifiedBakedInputs, environment_dependencies::{
        EnvironmentDependentPinOwner, EnvironmentDependentRead, RetainedEnvironmentSubgraph,
        RetainedSourceParent, SourceOverlayPublication, SourceOverlayPinOwner, SourceOverlayRead, SourceOverlayWorkData}, ImageImportFence, ImportPolicy, LocalStore, StagingLease},
    vfs::{HardlinkTopology, NodeKind, ObservationBudget, VfsCheckpoint, VfsManager, VfsView},
};
use rudder_vcs_bridge::{initial_source::PreparedInitialSource, seed::RetainedSeedCasCache};
use serde_json::{json, Value};
use std::{collections::{BTreeMap, BTreeSet}, fmt::Write as _, fs::{self, File, OpenOptions}, io::{Read, Write},
    os::unix::fs::{DirBuilderExt, FileExt, MetadataExt, OpenOptionsExt}, path::Path,
    sync::Arc, time::{Duration, Instant}};

const FORKS: usize = 50;
const CORPUS_HEAD: &str = "486d6c3cd5811bc27ddaa3fb61305ef7fadeb81c";
const ATTEMPTS: usize = FORKS + 2;
const WAVE_WIDTH: usize = 1;
const CAMPAIGN_SECONDS: u64 = 300;
const SCRATCH_BYTES: usize = 16 * 1024;
const DIAGNOSTIC_BYTES: usize = 2048;
const DIAGNOSTIC_FDS: usize = 65_536;

// Error DATA only. Formatting stops at the UTF-8 byte bound without dropping
// the actual error/publication carrier or parsing text into an errno/proof.
#[derive(Default)]
struct DiagnosticText { text: String, truncated: bool }
impl std::fmt::Write for DiagnosticText {
    fn write_str(&mut self, value: &str) -> std::fmt::Result {
        let left = DIAGNOSTIC_BYTES.saturating_sub(self.text.len());
        let mut end = value.len().min(left);
        while !value.is_char_boundary(end) { end -= 1; }
        self.text.push_str(&value[..end]);
        if end != value.len() { self.truncated = true; return Err(std::fmt::Error); }
        Ok(())
    }
}
pub(super) fn error_data(error: &anyhow::Error) -> Value {
    let mut text = DiagnosticText::default();
    let _ = write!(&mut text, "{error:#}");
    let io = error.chain().take(8).find_map(|cause|cause.downcast_ref::<std::io::Error>());
    json!({"detail":text.text,"truncated":text.truncated,"maximumUtf8Bytes":DIAGNOSTIC_BYTES,
        "ioRawOsError":io.and_then(std::io::Error::raw_os_error),
        "ioKind":io.map(|error|format!("{:?}",error.kind())),"diagnosticOnly":true})
}
// Failure-only fixture observations. No CAS/retention access or new clock:
// an unknown publication may retain the original GC lock and expired budget.
pub(super) fn source_failure_resource_data() -> Value {
    let mut data=json!({"diagnosticOnly":true,"fdCountIncludesObservationDirectory":true,
        "maximumFdEntriesObserved":DIAGNOSTIC_FDS});
    let mut limit=std::mem::MaybeUninit::<libc::rlimit>::uninit();
    if unsafe{libc::getrlimit(libc::RLIMIT_NOFILE,limit.as_mut_ptr())}==0 {
        let limit=unsafe{limit.assume_init()};
        data["rlimitNofile"]=json!({"soft":limit.rlim_cur.to_string(),"hard":limit.rlim_max.to_string(),
            "infinityValue":libc::RLIM_INFINITY.to_string()});
    }else{data["rlimitObservationError"]=error_data(&std::io::Error::last_os_error().into());}
    let result=(||->Result<()> {
        let mut count=0usize;
        for entry in fs::read_dir("/proc/self/fd")?.take(DIAGNOSTIC_FDS+1){entry?;count+=1;}
        data["fdEntriesObserved"]=json!(count.min(DIAGNOSTIC_FDS));
        data["fdCountExact"]=json!(count<=DIAGNOSTIC_FDS);Ok(())
    })();
    if let Err(error)=result{data["fdObservationError"]=error_data(&error);}
    data
}
fn resource_data(store: &LocalStore, clock: &Clock, stage: Option<&StagingLease>) -> Value {
    // These observations never acquire the CAS lock: a failed publication may
    // still own it. No retention readback, clock renewal, or FD-limit changes.
    let mut data = json!({"diagnosticOnly":true,"fdCountIncludesObservationDirectory":true,
        "maximumFdEntriesObserved":DIAGNOSTIC_FDS,
        "stagingExpiresAtUnixMs":stage.map(|s|s.expires_at_ms)});
    let mut count = 0usize;
    let result = (||->Result<()> {
        data["originalRemainingMs"] = json!(clock.remaining()?.as_millis());
        let mut limit = std::mem::MaybeUninit::<libc::rlimit>::uninit();
        ensure!(unsafe {libc::getrlimit(libc::RLIMIT_NOFILE,limit.as_mut_ptr())} == 0,
            "actual NOFILE observation failed: {}", std::io::Error::last_os_error());
        let limit = unsafe {limit.assume_init()};
        data["rlimitNofile"] = json!({"soft":limit.rlim_cur.to_string(),"hard":limit.rlim_max.to_string(),
            "infinityValue":libc::RLIM_INFINITY.to_string()});
        for entry in fs::read_dir("/proc/self/fd")?.take(DIAGNOSTIC_FDS+1) {
            if count % 128 == 0 { clock.remaining()?; }
            entry?; count += 1;
            ensure!(count <= DIAGNOSTIC_FDS,"FD diagnostic observation bound reached");
        }
        data["fdCountExact"] = json!(true);
        clock.remaining()?;
        let retention = fs::symlink_metadata(store.root().join("retention.json"))?;
        ensure!(retention.is_file()&&!retention.file_type().is_symlink(),"retention diagnostic file type changed");
        data["retentionEnvelopeBytes"] = json!(retention.len());
        data["observedUnixMs"] = json!(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_millis());
        clock.remaining()?; Ok(())
    })();
    data["fdEntriesObserved"] = json!(count);
    if let Err(error) = result { data["observationError"] = error_data(&error); }
    data
}

#[test]
fn bounded_diagnostic_preserves_utf8_and_only_actual_error_errno() {
    let error=anyhow::Error::new(std::io::Error::from_raw_os_error(libc::EMFILE))
        .context("actual IO error retained by the caller");
    let data=error_data(&error);
    assert_eq!(data["ioRawOsError"],libc::EMFILE);
    assert!(data["detail"].as_str().unwrap().contains("actual IO error retained"));
    assert_eq!(data["truncated"],false);
    let text_only=error_data(&anyhow::anyhow!("Too many open files (os error 24)"));
    assert!(text_only["ioRawOsError"].is_null());
    let large=error_data(&anyhow::anyhow!("é".repeat(DIAGNOSTIC_BYTES+1)));
    let text=large["detail"].as_str().unwrap();
    assert_eq!(text.len(),DIAGNOSTIC_BYTES);
    assert_eq!(text.chars().count(),DIAGNOSTIC_BYTES/2);
    assert_eq!(large["truncated"],true);
}

#[test]
fn cancelled_resource_observation_keeps_original_restriction_and_has_no_new_io_count() {
    let clock=Clock::new();let original=clock.0.constraint();clock.0.cancel();
    // A path is never opened or promoted to custody here: the cancelled
    // original is checked before any descriptor/retention observation.
    let temporary=std::env::temp_dir().join(format!("rudder-uncreated-diagnostic-{}",uuid::Uuid::new_v4()));
    let store=LocalStore::open(&temporary).unwrap();
    let data=resource_data(&store,&clock,None);
    assert_eq!(clock.0.constraint(),original);
    assert!(data.get("observationError").is_some());
    assert_eq!(data["fdEntriesObserved"],0);
    assert!(data.get("fdCountExact").is_none());
    assert!(data.get("retentionEnvelopeBytes").is_none());
    drop(store);fs::remove_dir_all(&temporary).unwrap();
}
#[derive(Clone, Copy, Debug)]
enum Purpose { Completed(usize), Failed, Cancelled }
struct Attempt {
    id: String, purpose: Purpose, clock: Arc<Clock>, constraint: Value,
    environment: Arc<RetainedEnvironmentSubgraph>, parent: Arc<EnvironmentDependentPinOwner>,
    source_parent: Arc<RetainedSourceParent>,
    seed: Arc<RetainedSeedCasCache>, view: Option<VfsView>, scratch: Option<File>,
    snapshot: Option<VfsCheckpoint>, stage: Option<StagingLease>, source: Option<ObjectRef>,
    runnable: Option<ObjectRef>, publication: Option<SourceOverlayPublication>,
    published: Option<Arc<SourceOverlayPinOwner>>, read: Option<SourceOverlayRead>,
    inherited_read: Option<SourceOverlayRead>,
    prefix: Vec<u8>, phase: &'static str, times: BTreeMap<&'static str, f64>,
    read_bytes: u64, scratch_bytes: u64, fork_private_bytes: u64, private_bytes: u64,
    expected_failure: bool,
    before_publication: Option<Value>, after_attempt: Option<Value>, publication_failure: Option<Value>,
}
impl Attempt {
    fn new(purpose: Purpose, campaign: Instant, environment: Arc<RetainedEnvironmentSubgraph>,
        parent: Arc<EnvironmentDependentPinOwner>, source_parent: Arc<RetainedSourceParent>,
        seed: Arc<RetainedSeedCasCache>) -> Result<Self> {
        // Capture the actual unchanged <=30s restriction BEFORE this wave's IO.
        let clock = restriction(campaign)?;
        let constraint = serde_json::to_value(clock.0.constraint())?;
        let prefix = match purpose {
            Purpose::Completed(n) => format!("rudder-private-fork-{n:02}"),
            Purpose::Failed => "rudder-negative-failed".into(),
            Purpose::Cancelled => "rudder-negative-cancelled".into(),
        }.into_bytes();
        Ok(Self { id: uuid::Uuid::new_v4().to_string(), purpose, clock, constraint,
            environment, parent, source_parent, seed, view: None, scratch: None, snapshot: None,
            stage: None, source: None, runnable: None, publication: None, published: None,
            read: None, inherited_read: None, prefix, phase: "staged-before-effects", times: BTreeMap::new(),
            read_bytes: 0, scratch_bytes: 0, fork_private_bytes: 0, private_bytes: 0,
            expected_failure: false, before_publication: None, after_attempt: None, publication_failure: None })
    }
    fn data(&self) -> Value {
        json!({"viewId":self.id,"purpose":format!("{:?}",self.purpose),"phase":self.phase,
            "originalConstraint":self.constraint,"timingsMs":self.times,
            "logicalReadBytes":self.read_bytes,"scratchBytes":self.scratch_bytes,
            "privatePayloadBytesImmediatelyAfterFork":self.fork_private_bytes,
            "privatePayloadBytesAfterEdit":self.private_bytes,
            "snapshot":self.snapshot,"source":self.source,"runnable":self.runnable,
            "actualPublicationAcknowledged":self.published.is_some(),
            "publicationProfile":"retained-source-overlay-v1",
            "sourceParentBindingId":self.source_parent.binding_data().and_then(|b|b.id_data()).ok(),
            "publishedDependencyDepth":self.published.as_ref().map(|p|p.record_data().depth),
            "publishedSourceParentBindingId":self.published.as_ref().map(|p|&p.record_data().parent_id),
            "sameActualSourceParentArc":self.published.as_ref().map(|p|Arc::ptr_eq(p.source_parent(),&self.source_parent)),
            "sameOriginalFlatSourceOwnerArc":Arc::ptr_eq(self.source_parent.original_owner(),&self.parent),
            "sameActualEnvironmentArc":Arc::ptr_eq(self.source_parent.original_owner().environment(),&self.environment),
            "overlayPublicationWork":self.publication.as_ref().map(|p|work_data(p.work_data())),
            "verifiedPublishedRunnableRead":self.read.as_ref().is_some_and(|r|r.verified_bytes().is_ok()),
            "verifiedInheritedCargoRead":self.inherited_read.as_ref().is_some_and(|r|r.verified_bytes().is_ok()),
            "expectedFailureObserved":self.expected_failure,
            "beforePublicationResources":self.before_publication,"afterAttemptResources":self.after_attempt,
            "retainedPublicationFailure":self.publication_failure,
            "originalOwnersRetained":true,"physicalWriterDrainClaim":false})
    }
}
fn work_data(work: SourceOverlayWorkData)->Value {
    json!({"verifiedDeltaObjects":work.verified_delta_objects,"verifiedDeltaBytes":work.verified_delta_bytes,
        "reusedParentEdges":work.reused_parent_edges,"sourceEntries":work.source_entries,"sourceLogicalBytes":work.source_logical_bytes,
        "actualIoComplete":work.actual_io_complete,"actualLocalHashReads":work.actual_local_hash_reads,
        "actualLocalHashBytes":work.actual_local_hash_bytes,"actualLocalObjectSyncs":work.actual_local_object_syncs,
        "actualLocalObjectSyncBytes":work.actual_local_object_sync_bytes,
        "scope":"successful-original-publication-local-object-calls-only",
        "excludesRetentionFilesDirectorySyncsForeignReadsAndEarlierParentWork":true,
        "perReferenceInheritedIoSeparatelyTagged":false})
}
struct Owners {
    manager: Option<VfsManager>, environment: Arc<RetainedEnvironmentSubgraph>,
    parent: Arc<EnvironmentDependentPinOwner>, source_parent: Option<Arc<RetainedSourceParent>>,
    inputs: Arc<VerifiedBakedInputs>,
    parent_read: Option<EnvironmentDependentRead>, attempts: Vec<Attempt>,
}
fn restriction(start: Instant) -> Result<Arc<Clock>> {
    let left = Duration::from_secs(CAMPAIGN_SECONDS).checked_sub(start.elapsed())
        .context("original fifty-fork campaign restriction expired")?;
    ensure!(!left.is_zero(), "original campaign expired before next operation");
    Ok(Arc::new(Clock(FinalizationBudget::begin(left.min(Duration::from_secs(30)))?)))
}
#[derive(Default, Clone, Copy)]
struct Disk { files: u64, bytes: u64, allocated: u64 }
fn disk(root: &Path) -> Result<Disk> {
    let mut value = Disk::default();
    let mut pending = vec![root.to_path_buf()];
    let mut seen = BTreeSet::new();
    let mut entries = 0;
    while let Some(path) = pending.pop() {
        for item in fs::read_dir(path)? {
            let item = item?; let m = fs::symlink_metadata(item.path())?;
            entries += 1; ensure!(entries <= 100_000, "benchmark disk metric entry bound");
            ensure!(!m.file_type().is_symlink(), "unexpected mutable metric symlink");
            if m.is_dir() { pending.push(item.path()); }
            else {
                ensure!(m.is_file(), "metric special file refused");
                if seen.insert((m.dev(), m.ino())) {
                    value.files += 1; value.bytes += m.len(); value.allocated += m.blocks() * 512;
                }
            }
        }
    }
    Ok(value)
}
fn proc_io() -> Result<BTreeMap<String, u64>> {
    let mut text = String::new();
    File::open("/proc/self/io")?.take(8193).read_to_string(&mut text)?;
    ensure!(text.len() <= 8192, "kernel IO metric bound");
    let mut data = BTreeMap::new();
    for line in text.lines() {
        let (key, value) = line.split_once(':').context("kernel IO metric malformed")?;
        if ["rchar", "wchar", "read_bytes", "write_bytes", "syscr", "syscw"].contains(&key) {
            data.insert(key.to_owned(), value.trim().parse()?);
        }
    }
    ensure!(data.len() == 6, "kernel IO counters absent"); Ok(data)
}
fn delta(before: &BTreeMap<String, u64>, after: &BTreeMap<String, u64>) -> Result<BTreeMap<String, u64>> {
    after.iter().map(|(key, end)| Ok((key.clone(), end.checked_sub(before[key])
        .context("kernel IO counter went backwards")?))).collect()
}
fn usage() -> Result<(f64,f64,i64)> {
    let mut actual=std::mem::MaybeUninit::<libc::rusage>::zeroed();
    ensure!(unsafe{libc::getrusage(libc::RUSAGE_SELF,actual.as_mut_ptr())}==0,"actual usage metric unavailable");
    let actual=unsafe{actual.assume_init()};
    Ok((actual.ru_utime.tv_sec as f64+actual.ru_utime.tv_usec as f64/1e6,
        actual.ru_stime.tv_sec as f64+actual.ru_stime.tv_usec as f64/1e6,actual.ru_maxrss as i64))
}
fn emit(path: &Path, value: &Value) -> Result<()> {
    let pending = path.with_extension(format!("pending-{}", uuid::Uuid::new_v4()));
    let mut f = OpenOptions::new().write(true).create_new(true).mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(&pending)?;
    f.write_all(&serde_json::to_vec_pretty(value)?)?; f.sync_all()?;
    fs::rename(pending, path)?; File::open(path.parent().unwrap())?.sync_all()?; Ok(())
}
fn samples(values: impl Iterator<Item=f64>) -> Value {
    let mut values = values.collect::<Vec<_>>(); values.sort_by(f64::total_cmp);
    if values.is_empty() { return json!({"samples":0}); }
    let rank = |p: f64| values[((p * values.len() as f64).ceil() as usize).saturating_sub(1).min(values.len()-1)];
    json!({"samples":values.len(),"minMs":values[0],"maxMs":values.last(),
        "p50Ms":if values.len()>=2 {Some(rank(0.50))}else{None},
        "p95Ms":if values.len()>=20 {Some(rank(0.95))}else{None},
        "p99Ms":if values.len()>=100 {Some(rank(0.99))}else{None},
        "p99NotQualified":values.len()<100,"rawMs":values})
}
fn root_blob(store: &LocalStore, directory: &ObjectRef, name: &[u8]) -> Result<ObjectRef> {
    let reader = LocalDirectoryStore::read_only(store);
    let entry = PagedDirectory::new(&reader).lookup(directory, name)?.context("actual corpus file absent")?;
    let EntryNode::File {object,logical_size,..} = entry.node else { anyhow::bail!("corpus entry must be file") };
    ensure!(object.kind == ObjectKind::Blob && logical_size == object.size_bytes
        && (64..=1024*1024).contains(&logical_size), "bounded genuine corpus blob required");
    Ok(object)
}
fn root_file(store: &LocalStore, directory: &ObjectRef, name: &[u8]) -> Result<Vec<u8>> {
    let object=root_blob(store,directory,name)?;
    let raw=store.read_object(&object)?;ensure!(raw.len() as u64==object.size_bytes,"corpus file length differs");Ok(raw)
}
fn private_bytes(manager: &VfsManager, id: &str) -> Result<u64> {
    Ok(disk(&manager.root().join("views").join(id).join("files"))?.bytes)
}
fn local_runtime_objects(store: &LocalStore, inputs: &VerifiedBakedInputs) -> Result<BTreeSet<ObjectRef>> {
    let mut present = BTreeSet::new();
    for reference in inputs.objects_data()? {
        match fs::symlink_metadata(store.object_path(reference)?) {
            Ok(meta) => {ensure!(meta.is_file()&&!meta.file_type().is_symlink(),"unsafe local runtime metric object");present.insert(reference.clone());},
            Err(error) if error.kind()==std::io::ErrorKind::NotFound => {},
            Err(error) => return Err(error.into()),
        }
    }
    Ok(present)
}
fn run_attempt(a: &mut Attempt, manager: &VfsManager, store: &LocalStore, scratch: &Path,
    parent: &RunnableRoot, source: &SourceV2, base: &VfsCheckpoint,
    readme: &[u8], cargo: &[u8]) -> Result<()> {
    let entire = Instant::now(); a.clock.remaining()?;
    a.phase = "fork-before-call"; let step = Instant::now();
    // This epoch scopes only this logical component's private VFS descriptors.
    // It is never a project/PG/Root/task authority receipt.
    a.view = Some(manager.fork_paged_snapshot(&a.id, base,
        OwnerEpoch {owner:"fifty-fork-storage-component-only".into(),epoch:1})?);
    a.clock.remaining()?; a.times.insert("fork", step.elapsed().as_secs_f64()*1000.0);
    let view = a.view.as_ref().unwrap().clone();
    let descriptor = view.descriptor()?;
    ensure!(descriptor.base == base.directory && descriptor.inode_count == 1
        && descriptor.private_dirent_count == 0, "fork enumerated/materialized inherited namespace");
    a.fork_private_bytes = private_bytes(manager, &a.id)?;
    ensure!(a.fork_private_bytes == 0, "fork copied inherited payload");
    a.phase = "first-real-inherited-read"; let step = Instant::now();
    let file = view.lookup(1,b"README.md")?;
    let got = view.read(file.ino,0,64)?; a.read_bytes += got.len() as u64;
    ensure!(got == readme[..64], "fork did not inherit the actual accepted README");
    let untouched = view.lookup(1,b"Cargo.toml")?;
    let got = view.read(untouched.ino,0,64)?; a.read_bytes += got.len() as u64;
    ensure!(got == cargo[..64], "fork did not inherit actual Cargo metadata");
    a.clock.remaining()?; a.times.insert("firstInheritedRead",step.elapsed().as_secs_f64()*1000.0);
    a.phase = "private-copy-on-write-and-scratch"; let step = Instant::now();
    ensure!(view.write(file.ino,0,&a.prefix,true)? == a.prefix.len(), "private edit short write");
    let private = scratch.join(&a.id); fs::DirBuilder::new().mode(0o700).create(&private)?;
    a.scratch = Some(OpenOptions::new().read(true).write(true).create_new(true).mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(private.join("build-scratch"))?);
    let bytes = (0..SCRATCH_BYTES).map(|n| a.prefix[n%a.prefix.len()]).collect::<Vec<_>>();
    a.scratch.as_mut().unwrap().write_all(&bytes)?; a.scratch.as_ref().unwrap().sync_all()?;
    File::open(&private)?.sync_all()?; a.scratch_bytes = bytes.len() as u64;
    a.private_bytes = private_bytes(manager,&a.id)?;
    ensure!(a.private_bytes == 64*1024, "one-block source edit did not use actual64KiB range CoW");
    let got=view.read(file.ino,0,a.prefix.len())?;a.read_bytes+=got.len()as u64;
    ensure!(got==a.prefix,"private source edit differs");
    a.clock.remaining()?;a.times.insert("editAndScratch",step.elapsed().as_secs_f64()*1000.0);
    match a.purpose {
        Purpose::Failed => {
            let old=view.descriptor()?;let bytes=private_bytes(manager,&a.id)?;
            let error=view.create(1,b"README.md",NodeKind::File,0o600,b"").err().context("expected actual inherited EEXIST failure")?;
            ensure!(rudder_engine::vfs::errno(&error)==libc::EEXIST,"different actual namespace error");
            ensure!(view.descriptor()?.private_dirent_count==old.private_dirent_count
                &&private_bytes(manager,&a.id)?==bytes,"failed create changed acknowledged private state");
            a.clock.remaining()?;a.expected_failure=true;a.phase="known-EEXIST-private-work-retained";
            a.times.insert("total",entire.elapsed().as_secs_f64()*1000.0);return Ok(());
        }
        Purpose::Cancelled => {
            // Exercise the REAL cancellable source consumer, not a synthetic
            // ready flag. Its first cooperative check precedes DB/staging IO.
            let budget=ObservationBudget {max_elapsed_ms:a.clock.remaining()?.as_millis().max(1).min(30_000)as u64,
                ..ObservationBudget::default()};
            a.clock.0.cancel();
            let result=view.observe_source_cancellable(&ImportPolicy::default(),&a.id,&budget,
                &||a.clock.remaining().is_err());
            ensure!(result.is_err()&&a.clock.remaining().is_err(),"actual original cancellation was not refused");
            a.expected_failure=true;a.phase="cancelled-source-observation-private-work-retained";
            a.times.insert("total",entire.elapsed().as_secs_f64()*1000.0);return Ok(());
        }
        Purpose::Completed(_) => {}
    }
    a.phase="seal-original-unmounted-logical-writers";let step=Instant::now();
    a.snapshot=Some(view.seal()?);a.clock.remaining()?;
    ensure!(rudder_engine::vfs::errno(&view.write(file.ino,0,b"MUST_REFUSE",true).unwrap_err())==libc::EROFS,
        "sealed logical fork still admits writes");
    a.times.insert("seal",step.elapsed().as_secs_f64()*1000.0);
    a.phase="stage-and-publish-actual-source-overlay-Runnable";let step=Instant::now();
    a.stage=Some(store.begin_staging(a.clock.remaining()?)?);
    let stage=&a.stage.as_ref().unwrap().id;let snapshot=a.snapshot.as_ref().unwrap();
    a.source=Some(snapshot::stage(store,stage,ObjectKind::SourceRoot,&SourceV2{format_version:2,
        consistency:CheckpointConsistency::Quiesced,directory:snapshot.directory.clone(),projection_policy:source.projection_policy.clone()})?);
    a.runnable=Some(snapshot::stage(store,stage,ObjectKind::RunnableRoot,&RunnableRoot{format_version:1,
        consistency:CheckpointConsistency::Quiesced,source:a.source.as_ref().unwrap().clone(),
        environment:Some(a.environment.root_data()?.clone()),view_root:snapshot.directory.clone(),
        profile:parent.profile.clone(),hardlinks:snapshot.hardlinks.clone(),knowledge_cursor:parent.knowledge_cursor})?);
    a.before_publication = Some(resource_data(store,&a.clock,a.stage.as_ref()));
    let result = store.publish_source_overlay_retaining(&mut a.publication,stage.clone(),format!("fifty:fork:{}",a.id),
        a.runnable.as_ref().unwrap().clone(),a.source_parent.clone(),a.clock.clone());
    if let Err(error) = result {
        // Capture the EXISTING retained original cause immediately. The public
        // call's generic error must not erase an actual IO/retention refusal.
        a.publication_failure = a.publication.as_ref().and_then(|p|p.failure()).map(error_data);
        a.times.insert("dependentPublishBeforeFailure",step.elapsed().as_secs_f64()*1000.0);
        a.times.insert("totalBeforeFailure",entire.elapsed().as_secs_f64()*1000.0);
        return Err(error);
    }
    a.published=Some(a.publication.as_ref().unwrap().acknowledgment()?.clone()); // BEFORE fallible postchecks.
    a.clock.remaining()?;
    ensure!(serde_json::to_value(a.clock.0.constraint())?==a.constraint,"fork original constraint changed");
    let owner=a.published.as_ref().unwrap();
    ensure!(Arc::ptr_eq(owner.source_parent(),&a.source_parent)
        &&Arc::ptr_eq(owner.source_parent().original_owner(),&a.parent)
        &&Arc::ptr_eq(owner.source_parent().original_owner().environment(),&a.environment),"fork lost actual shared Source/Environment owners");
    let work=a.publication.as_ref().unwrap().work_data();
    ensure!(work.actual_io_complete&&work.verified_delta_objects>0&&work.reused_parent_edges>0
        &&work.actual_local_hash_reads==work.verified_delta_objects
        &&work.actual_local_hash_bytes==work.verified_delta_bytes
        &&work.actual_local_object_syncs==work.verified_delta_objects
        &&work.actual_local_object_sync_bytes==work.verified_delta_bytes,"actual successful local object IO differs from verified new delta");
    a.times.insert("dependentPublish",step.elapsed().as_secs_f64()*1000.0);
    a.times.insert("total",entire.elapsed().as_secs_f64()*1000.0);a.phase="actual-immutable-publication-acknowledged";Ok(())
}

pub(super) fn run(output:&Path,store:&LocalStore,source:&PreparedInitialSource,
    environment:Arc<RetainedEnvironmentSubgraph>,inputs:Arc<VerifiedBakedInputs>)->Result<()> {
    let campaign=Instant::now();let root=output.join("fifty-forks");
    ensure!(source.baseline.commit.0==CORPUS_HEAD,"fifty-fork benchmark requires the previously proven accepted Rudder corpus");
    fs::DirBuilder::new().mode(0o700).create(&root)?;
    let parent=source.runnable_dependency().context("actual original dependent source owner absent")?.clone();
    ensure!(Arc::ptr_eq(parent.environment(),&environment),"benchmark Environment differs from actual source");
    // Staged BEFORE manager/fork/read/publication effects; retained even on panic.
    let held=Box::leak(Box::new(Owners{manager:None,environment,parent,source_parent:source.source_parent().cloned(),inputs,parent_read:None,attempts:Vec::new()}));
    let mut report=json!({"formatVersion":1,"scope":"actual-full-Rudder-logical-source-CoW-and-retained-runtime-component-only",
        "passed":false,"requestedCompletedForks":FORKS,"attemptCap":ATTEMPTS,"waveWidth":WAVE_WIDTH,
        "maximumConcurrentMutatingJobs":1,"campaignRestrictionSeconds":CAMPAIGN_SECONDS,"originalOperationLimitMs":30000,
        "head":source.baseline.commit,"operation":source.operation,"seed":source.seed_root,
        "actualAgentLaunch":false,"RootMountAuthority":false,"privateJjForksQualified":false,
        "normalDrain":false,"compilerSDK":false,"coldMedia":false,"coldWorkerPopulationQualified":false,
        "networkDataPlaneConfigured":false,"remoteFetchBytes":0,"cancelIsProcessStop":false,
        "logicalVfsIoPreemptivelyBounded":false,"payloadHashReadsSeparatelyInstrumented":true,
        "actualLocalObjectSyncsSeparatelyInstrumented":true,"sourceOverlayDependencyDepth":3,
        "originalWholeComponentLimitSeconds":480,"nofileLimitChangedByFixture":false,
        "payloadIoScope":"original-overlay-publication-local-objects-only",
        "sourceParentDerivationIoIncludedInPublicationCounters":false,"perReferenceInheritedIoSeparatelyTagged":false});
    let result=(||->Result<()> {
        let clock=restriction(campaign)?;
        // Retain the genuine parent already derived by original initial-source
        // preparation; setup and all 52 attempts reuse this exact Arc.
        let setup_constraint=clock.0.constraint();let capture_start=Instant::now();
        let shared_parent=held.source_parent.as_ref().context("prepared initial Source parent absent")?;
        ensure!(Arc::ptr_eq(shared_parent,source.source_parent().context("original prepared Source parent absent")?)
            &&Arc::ptr_eq(shared_parent.original_owner(),&held.parent)
            &&Arc::ptr_eq(shared_parent.original_owner().environment(),&held.environment)
            &&clock.0.constraint()==setup_constraint,"prepared Source parent changed original custody/clock");
        report["sourceParentBinding"]=json!(shared_parent.binding_data()?);clock.remaining()?;
        report["capturePreparedSourceParentMs"]=json!(capture_start.elapsed().as_secs_f64()*1000.0);
        // This one derivation belongs to original initial-source preparation;
        // the fixture captures that completed fact and performs no derivation.
        report["sourceParentDerivations"]=json!(1);
        report["sourceParentCapture"]=json!("prepared-initial-source");
        report["samePreparedSourceParentArc"]=json!(true);
        held.parent.read_retaining(&mut held.parent_read,source.runnable_root.clone(),clock.clone())?;
        let parent=snapshot::decode_runnable(held.parent_read.as_ref().unwrap().verified_bytes()?)?;
        let original_source=snapshot::decode_source(&store.read_object(&source.source_root)?)?;
        ensure!(parent.source==source.source_root&&parent.view_root==original_source.directory
            &&parent.environment.as_ref()==Some(held.environment.root_data()?),"actual accepted parent/Environment differs");
        let topology:HardlinkTopology=serde_json::from_slice(&store.read_object(&parent.hardlinks)?)?;
        ensure!(topology.groups.is_empty(),"accepted SourceOnly parent topology contract changed");
        let base=VfsCheckpoint{format_version:1,directory:parent.view_root.clone(),hardlinks:parent.hardlinks.clone()};
        let readme=root_file(store,&base.directory,b"README.md")?;let cargo=root_file(store,&base.directory,b"Cargo.toml")?;
        let cargo_object=root_blob(store,&base.directory,b"Cargo.toml")?;
        let start=Instant::now();held.manager=Some(VfsManager::open(root.join("manager"),store.clone())?);
        held.manager.as_ref().unwrap().prepare_paged_snapshot(&base)?;clock.remaining()?;
        report["prepareSourceVfsMs"]=json!(start.elapsed().as_secs_f64()*1000.0);
        let manager=held.manager.as_ref().unwrap().clone();
        let scratch=root.join("scratch");fs::DirBuilder::new().mode(0o700).create(&scratch)?;
        let before=disk(&store.root().join("objects"))?;let initial_state=store.retention()?;
        let original_count=initial_state.environment_dependencies.as_ref().context("original dependency namespace absent")?.pins.len();
        ensure!(initial_state.source_dependencies.is_none(),"fresh sibling campaign unexpectedly has prior Source overlays");
        let before_foreign=local_runtime_objects(store,&held.inputs)?;
        let io_before=proc_io()?;
        let usage_before=usage()?;
        clock.remaining()?;
        for ordinal in 0..FORKS {
            let mut purposes=vec![Purpose::Completed(ordinal)];
            if ordinal==FORKS/2 {purposes.extend([Purpose::Failed,Purpose::Cancelled]);}
            for purpose in purposes {
                ensure!(held.attempts.len()<ATTEMPTS,"fixed benchmark attempt bound");
                held.attempts.push(Attempt::new(purpose,campaign,held.environment.clone(),held.parent.clone(),held.source_parent.as_ref().unwrap().clone(),source.seed_cache().clone())?);
                let actual=held.attempts.last_mut().unwrap();
                // No retry/reconstruction if the original view/publication errs.
                let result=run_attempt(actual,&manager,store,&scratch,&parent,&original_source,&base,&readme,&cargo);
                actual.after_attempt=Some(resource_data(store,&actual.clock,actual.stage.as_ref()));
                if let Err(error)=result{
                    report["failedAttempt"]=actual.data();return Err(error);}
                report["attempts"]=json!(held.attempts.iter().map(Attempt::data).collect::<Vec<_>>());
                emit(&root.join("result.json"),&report)?;
            }
        }
        let mut roots=BTreeSet::new();let mut directories=BTreeSet::new();let mut ids=BTreeSet::new();let mut logical_read=0;
        for a in &mut held.attempts {
            let clock=restriction(campaign)?;let view=a.view.as_ref().context("actual original fork absent")?;
            ensure!(ids.insert(a.id.clone())&&Arc::ptr_eq(&a.environment,&held.environment)
                &&Arc::ptr_eq(&a.parent,&held.parent)&&Arc::ptr_eq(&a.source_parent,held.source_parent.as_ref().unwrap())
                &&Arc::ptr_eq(&a.seed,source.seed_cache()),"fork identity/actual retained parent changed");
            let file=view.lookup(1,b"README.md")?;let bytes=view.read(file.ino,0,readme.len())?;
            ensure!(bytes.len()==readme.len()&&bytes[..a.prefix.len()]==a.prefix
                &&bytes[a.prefix.len()..]==readme[a.prefix.len()..],"sibling divergence/isolation failed");
            let file=view.lookup(1,b"Cargo.toml")?;ensure!(view.read(file.ino,0,cargo.len())?==cargo,"sibling corrupted untouched accepted file");
            let mut scratch_bytes=vec![0;SCRATCH_BYTES];
            a.scratch.as_ref().unwrap().read_exact_at(&mut scratch_bytes,0)?;
            ensure!(a.scratch.as_ref().unwrap().read_at(&mut [0],SCRATCH_BYTES as u64)?==0,"scratch grew beyond its acknowledged length");
            ensure!(scratch_bytes==(0..SCRATCH_BYTES).map(|n|a.prefix[n%a.prefix.len()]).collect::<Vec<_>>(),"private scratch leaked across siblings");
            logical_read+=a.read_bytes+readme.len()as u64+cargo.len()as u64;
            match a.purpose {
                Purpose::Completed(_) => {
                    let owner=a.published.as_ref().context("no actual source overlay publication ACK")?;
                    ensure!(roots.insert(owner.root_data().clone())&&directories.insert(a.snapshot.as_ref().unwrap().directory.clone()),"immutable fork roots were not distinct");
                    owner.read_retaining(&mut a.read,owner.root_data().clone(),clock.clone())?;
                    let actual=snapshot::decode_runnable(a.read.as_ref().unwrap().verified_bytes()?)?;
                    ensure!(actual.source==*a.source.as_ref().unwrap()&&actual.view_root==a.snapshot.as_ref().unwrap().directory
                        &&actual.environment.as_ref()==Some(held.environment.root_data()?)&&owner.record_data().depth==3
                        &&Arc::ptr_eq(owner.source_parent(),held.source_parent.as_ref().unwrap())
                        &&Arc::ptr_eq(owner.source_parent().original_owner(),&held.parent)
                        &&Arc::ptr_eq(owner.source_parent().original_owner().environment(),&held.environment),"published immutable fork lost exact shared Source/Environment");
                    owner.read_retaining(&mut a.inherited_read,cargo_object.clone(),clock.clone())?;
                    ensure!(a.inherited_read.as_ref().unwrap().verified_bytes()?==cargo,"overlay verified read lost original inherited Cargo payload");
                }
                Purpose::Failed|Purpose::Cancelled => ensure!(a.expected_failure&&a.published.is_none()
                    &&a.publication.is_none()&&a.runnable.is_none(),"negative attempt manufactured a publication"),
            }
            clock.remaining()?;
        }
        ensure!(roots.len()==FORKS&&directories.len()==FORKS&&ids.len()==ATTEMPTS,"actual fork count differs");
        let final_clock=restriction(campaign)?;
        ensure!(root_file(store,&base.directory,b"README.md")?==readme
            &&root_file(store,&base.directory,b"Cargo.toml")?==cargo,"accepted parent changed");
        ensure!(source.repository().op_id().hex()==source.operation.0,"accepted jj operation changed during private logical forks");
        let state=store.retention()?;let dependencies=state.environment_dependencies.as_ref().context("dependent retention disappeared")?;
        ensure!(dependencies.pins.len()==original_count&&dependencies.environments.len()==1&&dependencies.holds.is_empty(),"original flat Source/Environment obligations changed");
        let overlays=state.source_dependencies.as_ref().context("Source overlay namespace absent")?;
        let shared_parent=held.source_parent.as_ref().unwrap();
        ensure!(Arc::ptr_eq(shared_parent,source.source_parent().context("original prepared Source parent absent")?),
            "final Source parent differs from original initial-source preparation");
        let parent_binding=shared_parent.binding_data()?;let parent_id=parent_binding.id_data()?;
        ensure!(overlays.pins.len()==FORKS&&overlays.parents.len()==1&&overlays.holds.is_empty()
            &&overlays.parents.get(&parent_id)==Some(parent_binding),"Source overlay siblings/parent obligations differ");
        report["dependencyNamespaces"]=json!({"environmentPinsBefore":original_count,"environmentPinsAfter":dependencies.pins.len(),
            "environmentAnchors":dependencies.environments.len(),"environmentPendingHolds":dependencies.holds.len(),
            "sourceOverlayPins":overlays.pins.len(),"sourceParents":overlays.parents.len(),"sourcePendingHolds":overlays.holds.len(),
            "sharedSourceParentId":parent_id,"sourceOverlayDepth":3});
        ensure!(state.pins.get(&source.seed_pin)==Some(&source.seed_root)
            &&held.parent.matches_record_data(&state,&source.runnable_pin),"old source/history retention lost");
        for a in &held.attempts {if let Some(owner)=&a.published {ensure!(owner.matches_record_data(&state,&format!("fifty:fork:{}",a.id))
                &&owner.record_data().parent_id==parent_id&&owner.record_data().depth==3
                &&!dependencies.pins.contains_key(&format!("fifty:fork:{}",a.id))
                &&!state.pins.values().any(|r|r==owner.root_data()),"fork forged ordinary/flat pin or lost shared Source parent");}}
        let after_foreign=local_runtime_objects(store,&held.inputs)?;
        let newly_local=after_foreign.difference(&before_foreign).collect::<Vec<_>>();
        ensure!(newly_local.is_empty(),"fork copied runtime backing objects into writable CAS");
        let after=disk(&store.root().join("objects"))?;let private=disk(&manager.root().join("views"))?;let scratch_disk=disk(&scratch)?;
        let io_after=proc_io()?;let usage_after=usage()?;
        let completed=held.attempts.iter().filter(|a|matches!(a.purpose,Purpose::Completed(_))).collect::<Vec<_>>();
        let warm=completed.iter().skip(1).copied().collect::<Vec<_>>();
        report["passed"]=json!(true);report["completedImmutableForks"]=json!(roots.len());report["actualPrivateAttempts"]=json!(ids.len());
        report["allOriginalPrivateViewsRetained"]=json!(true);report["sameActualEnvironmentArc"]=json!(true);
        report["sameActualSourceParentArc"]=json!(true);report["sameOriginalFlatSourceOwnerArc"]=json!(true);
        report["successfulVerifiedOverlayRunnableReads"]=json!(completed.iter().filter(|a|a.read.as_ref().is_some_and(|r|r.verified_bytes().is_ok())).count());
        report["successfulVerifiedInheritedCargoReads"]=json!(completed.iter().filter(|a|a.inherited_read.as_ref().is_some_and(|r|r.verified_bytes().is_ok())).count());
        let mut hash_reads=0u64;let mut hash_bytes=0u64;let mut syncs=0u64;let mut sync_bytes=0u64;
        for a in &completed {let work=a.publication.as_ref().unwrap().work_data();
            ensure!(work.actual_io_complete,"successful publication IO metrics incomplete");
            hash_reads=hash_reads.checked_add(work.actual_local_hash_reads).context("hash-read metric overflow")?;
            hash_bytes=hash_bytes.checked_add(work.actual_local_hash_bytes).context("hash-byte metric overflow")?;
            syncs=syncs.checked_add(work.actual_local_object_syncs).context("sync metric overflow")?;
            sync_bytes=sync_bytes.checked_add(work.actual_local_object_sync_bytes).context("sync-byte metric overflow")?;}
        report["actualOverlayPublicationLocalIo"]=json!({"successfulPublications":completed.len(),"actualLocalHashReads":hash_reads,
            "actualLocalHashBytes":hash_bytes,"actualLocalObjectSyncs":syncs,"actualLocalObjectSyncBytes":sync_bytes,
            "allMatchVerifiedNewDelta":true,"perReferenceInheritedIoSeparatelyTagged":false,
            "excludesRetentionFilesDirectorySyncsForeignReadsEarlierParentAndServedReadWork":true});
        report["firstUseForkMs"]=json!(completed[0].times["fork"]);
        report["firstUseSampleCount"]=json!(1);report["coldForkPercentiles"]=json!(null);
        report["coldForkPercentilesReason"]=json!("one first-use VFS sample; no cold-media/worker population or independent cold trials");
        for key in ["fork","firstInheritedRead","editAndScratch","seal","dependentPublish","total"] {
            report[format!("warm_{key}")]=samples(warm.iter().map(|a|a.times[key]));
            report[format!("all_{key}")]=samples(completed.iter().map(|a|a.times[key]));
        }
        report["bytes"]=json!({"sharedRuntimeEncoded":held.inputs.binding_data()?.encoded_bytes,
            "sharedSourceEncoded":held.parent.record_data().local_bytes,"sharedSeedRoot":source.seed_root,
            "sourceHistoryImports":1,"runtimePayloadImportsPerFork":0,"newLocalRuntimeObjects":newly_local.len(),
            "newCasObjects":after.files.checked_sub(before.files).context("CAS count decreased")?,
            "newCasEncoded":after.bytes.checked_sub(before.bytes).context("CAS bytes decreased")?,
            "newCasAllocatedBlockBytes":after.allocated.checked_sub(before.allocated).context("CAS block bytes decreased")?,
            "privateViewsIncludingSqliteBytes":private.bytes,"privateViewsIncludingSqliteAllocatedBlockBytes":private.allocated,
            "privateRangeLogBytes":held.attempts.iter().map(|a|a.private_bytes).sum::<u64>(),
            "scratchBytes":scratch_disk.bytes,"scratchAllocatedBlockBytes":scratch_disk.allocated,
            "logicalSourceReadBytes":logical_read,"privateScratchReadBytes":ATTEMPTS*SCRATCH_BYTES,"kernelIoDelta":delta(&io_before,&io_after)?,
            "kernelCountersIncludeSqlitePublicationValidationAndObservationIo":true,
            "allocatedBlocksAreNotAReflinkExtentSharingProof":true});
        report["process"]=json!({"wholeProcessPeakRssBytesIncludingParentSetup":usage_after.2*1024,
            "benchmarkCpuUserSeconds":usage_after.0-usage_before.0,
            "benchmarkCpuSystemSeconds":usage_after.1-usage_before.1,
            "architecture":std::env::consts::ARCH,"availableParallelism":std::thread::available_parallelism()?.get()});
        final_clock.remaining()?;
        ensure!(campaign.elapsed()<Duration::from_secs(CAMPAIGN_SECONDS),"original benchmark restriction expired");Ok(())
    })();
    report["campaignElapsedMs"]=json!(campaign.elapsed().as_secs_f64()*1000.0);
    report["attempts"]=json!(held.attempts.iter().map(Attempt::data).collect::<Vec<_>>());
    report["retainedSourceParentFailure"]=held.source_parent.as_ref().and_then(|p|p.failure()).map(error_data).unwrap_or(Value::Null);
    if let Err(error)=&result {report["passed"]=json!(false);report["failure"]=json!({"retainedOriginals":true,"detail":format!("{error:#}").chars().take(2048).collect::<String>()});}
    if let Err(write_error)=emit(&root.join("result.json"),&report){
        // Descriptor/disk exhaustion must not erase the original retained
        // publication cause merely because another report file cannot open.
        report["passed"]=json!(false);report["reportWriteFailure"]=error_data(&write_error);
        println!("RUDDER_FIFTY_FORK_SOURCE_ENVIRONMENT_V1={report}");
        return match result {Err(original)=>Err(original),Ok(())=>Err(write_error)};
    }
    println!("RUDDER_FIFTY_FORK_SOURCE_ENVIRONMENT_V1={report}");result
}
