//! Local immutable preparation reuse, never scheduling or launch authority.
//! All methods and final owner/view drops are OFF-APP. Root retains the returned
//! floor through actual helper/worker settlement, not merely backend completion.
#![cfg(target_os="linux")]
use crate::{engine_host::EngineSourceLease, managed_launch::{ManagedLaunchFence,
    ManagedLocalLaunchRequest, ManagedRunSource}, managed_store::ManagedStore};
use anyhow::{ensure, Context, Result};
use rudder_engine::{execution::finalization_budget::FinalizationBudget,
    journal::Journal, protocol::{canonical_json, ObjectKind, ObjectRef},
    snapshot::{decode_environment, decode_runnable, decode_source, RunnableRoot},
    store::LocalStore, vfs::{VfsCheckpoint, VfsManager, VfsView}, workspace::WorkspaceProfile};
use rudder_vcs_bridge::vfs_provider::SourceProjectionPolicy;
use std::{fs::{self, File, OpenOptions}, os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf}, sync::{Arc, Mutex, MutexGuard, TryLockError,
        atomic::{AtomicBool, AtomicUsize, Ordering}}, time::{Duration, Instant}};

const CACHE_FORMAT: u32 = 1;
const PAGED_VIEW_FORMAT: u32 = 2;
const MAX_FLOORS: usize = 2;
const MAX_ADMISSIONS: usize = 2;
const WAIT_SLICE: Duration = Duration::from_millis(10);

struct Anchor { path: PathBuf, held: File, device: u64, inode: u64 }
impl Anchor {
    fn open(path: &Path) -> Result<Self> {
        let held = OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW
            | libc::O_NONBLOCK | libc::O_CLOEXEC).open(path)?;
        let meta = held.metadata()?;
        let value = Self { path:path.into(), held, device:meta.dev(), inode:meta.ino() };
        value.check()?; Ok(value)
    }
    fn check(&self) -> Result<()> {
        for meta in [self.held.metadata()?, fs::symlink_metadata(&self.path)?] {
            ensure!(meta.is_dir() && meta.uid() == unsafe { libc::geteuid() }
                && meta.mode() & 0o077 == 0 && (meta.dev(),meta.ino()) == (self.device,self.inode),
                "managed preparation directory custody changed");
        }
        Ok(())
    }
}
#[derive(Clone, PartialEq, Eq)]
struct Key { cache_format:u32, paged_format:u32, root:ObjectRef, runnable:RunnableRoot, snapshot:VfsCheckpoint }
#[derive(Clone)]
struct Floor { key:Key, metadata_bytes:u64,backed_inputs:Option<Arc<crate::managed_initial_source::task_backing::ManagedBackedTaskInputs>> }
struct State { ready:Vec<Floor>, preparations:u64 }

pub(crate) struct ManagedPreparationCache {
    project:ManagedStore, store:LocalStore, manager:VfsManager,
    anchors:Vec<Anchor>, state:Mutex<State>, active:AtomicUsize, ready:AtomicUsize, fenced:AtomicBool,
    // Last actual lease/checkout-lock holder drops AFTER manager/anchor IO.
    lease:EngineSourceLease,
}
struct Admission { cache:Arc<ManagedPreparationCache> }
impl Drop for Admission {
    fn drop(&mut self) { let old=self.cache.active.fetch_sub(1,Ordering::AcqRel); debug_assert!(old>0); }
}
/// Performance DATA only: counts the two top-level snapshot CAS refs, not all
/// source bytes, resident memory, physical IO saved, or model readiness.
#[derive(Debug, Clone)]
pub(crate) struct ManagedPreparationReuse {
    pub cache_hit:bool, pub preparation_calls:u64, pub cached_floors:usize,
    pub reused_floor_metadata_bytes:u64, pub wait_ns:u128, pub prepare_ns:u128, pub fork_ns:u128,
}
pub(crate) struct ManagedPreparedView {
    view:VfsView, // Drop the per-attempt connection before its actual cache owner.
    cache:Arc<ManagedPreparationCache>,
    stats:ManagedPreparationReuse,
}
impl ManagedPreparedView {
    pub(crate) fn manager(&self) -> &VfsManager { &self.cache.manager }
    pub(crate) fn view(&self) -> &VfsView { &self.view }
    pub(crate) fn stats(&self) -> &ManagedPreparationReuse { &self.stats }
}

impl ManagedPreparationCache {
    /// Actual owner-bound writer + lease, never serialized owner/path authority.
    /// Every open gets a fresh manager directory; restart never trusts a saved
    /// prepared marker as a cache hit. Root shares ONE Arc for this project.
    pub(crate) fn open(lease:EngineSourceLease, journal:&Journal, project:ManagedStore,
        store:LocalStore) -> Result<Arc<Self>> {
        lease.check_journal(journal)?;lease.check_shared_current()?;
        ensure!(lease.owner().project_id == project.binding().project_id
            && project.reopen_current(journal)? == project,
            "preparation cache differs from actual managed owner/project");
        let mut anchors=vec![Anchor::open(&project.binding().directory)?, Anchor::open(store.root())?,
            Anchor::open(&store.root().join("objects"))?];
        let parent=project.binding().directory.join("preparation-cache-v1");
        match fs::DirBuilder::new().mode(0o700).create(&parent) {
            Ok(()) => { File::open(&parent)?.sync_all()?; File::open(parent.parent().unwrap())?.sync_all()?; }
            Err(error) if error.kind()==std::io::ErrorKind::AlreadyExists => {},
            Err(error) => return Err(error.into()),
        }
        anchors.push(Anchor::open(&parent)?);
        let root=parent.join(format!("{}-{}",lease.owner().epoch,uuid::Uuid::new_v4()));
        fs::DirBuilder::new().mode(0o700).create(&root)?;
        anchors.push(Anchor::open(&root)?);
        let manager=VfsManager::open(&root,store.clone())?;
        File::open(&parent)?.sync_all()?;
        let value=Arc::new(Self { lease,project,store,manager,anchors,
            state:Mutex::new(State { ready:Vec::new(),preparations:0 }), active:AtomicUsize::new(0),
            ready:AtomicUsize::new(0),fenced:AtomicBool::new(false) });
        value.custody()?; Ok(value)
    }
    fn custody(&self) -> Result<()> {
        let checked=(|| {
            ensure!(!self.fenced.load(Ordering::Acquire),"managed preparation cache is fenced");
            self.lease.check_current()?;
            for anchor in &self.anchors { anchor.check()?; }
            ensure!(!self.fenced.load(Ordering::Acquire),"managed preparation cache is fenced"); Ok(())
        })();
        if checked.is_err() { self.fenced.store(true,Ordering::Release); }
        checked
    }
    fn admit(self:&Arc<Self>) -> Result<Admission> {
        let old=self.active.fetch_add(1,Ordering::AcqRel);
        if old>=MAX_ADMISSIONS { self.active.fetch_sub(1,Ordering::AcqRel);
            anyhow::bail!("managed preparation cache admission is busy"); }
        Ok(Admission { cache:Arc::clone(self) })
    }
    fn time(cancelled:&AtomicBool,budget:&FinalizationBudget) -> Result<Duration> {
        ensure!(!cancelled.load(Ordering::Acquire),"managed preparation cancelled"); budget.remaining()
    }
    fn before_effect(&self,cancelled:&AtomicBool,budget:&FinalizationBudget) -> Result<()> {
        Self::time(cancelled,budget)?; self.lease.check_active_for_adoption()?;
        ensure!(!self.fenced.load(Ordering::Acquire),"managed preparation cache is fenced"); Ok(())
    }
    fn lock<'a>(&'a self,cancelled:&AtomicBool,budget:&FinalizationBudget) -> Result<MutexGuard<'a,State>> {
        loop {
            let remaining=Self::time(cancelled,budget)?;
            self.lease.check_active_for_adoption()?;
            match self.state.try_lock() {
                Ok(state) => return Ok(state),
                Err(TryLockError::WouldBlock) => std::thread::sleep(remaining.min(WAIT_SLICE)),
                Err(TryLockError::Poisoned(_)) => anyhow::bail!("managed preparation state unavailable"),
            }
        }
    }
    /// REQUIRED budget was captured before queue/work. It is not a new StopV4,
    /// grant or helper startup interval, and this module never begins/renews it.
    pub(crate) fn fork(self:&Arc<Self>,request:&ManagedLocalLaunchRequest,journal:&Journal,
        cancelled:&AtomicBool,budget:&FinalizationBudget) -> Result<ManagedPreparedView> {
        ensure!(request.managed_store == self.project && request.store.root()==self.store.root(),
            "managed cache request changed project/CAS identity");
        self.fork_selected_retained(&request.fence,&request.runnable_root,&request.seed_source,
            &request.view_id,journal,cancelled,budget,request.backed_inputs.clone())
    }
    fn fork_selected(self:&Arc<Self>,fence:&ManagedLaunchFence,root:&ObjectRef,source:&ObjectRef,
        view_id:&str,journal:&Journal,cancelled:&AtomicBool,budget:&FinalizationBudget) -> Result<ManagedPreparedView> {
        self.fork_selected_retained(fence,root,source,view_id,journal,cancelled,budget,None)
    }
    fn fork_selected_retained(self:&Arc<Self>,fence:&ManagedLaunchFence,root:&ObjectRef,source:&ObjectRef,
        view_id:&str,journal:&Journal,cancelled:&AtomicBool,budget:&FinalizationBudget,
        backed_inputs:Option<Arc<crate::managed_initial_source::task_backing::ManagedBackedTaskInputs>>) -> Result<ManagedPreparedView> {
        let _admission=self.admit()?; Self::time(cancelled,budget)?; self.custody()?;
        ensure!(fence.owner==*self.lease.owner(),"managed cache owner differs from reserved attempt");
        self.lease.check_journal(journal)?;self.lease.check_shared_current()?; fence.check_current(journal)?;
        ensure!(uuid::Uuid::parse_str(view_id).is_ok_and(|id|id.to_string()==view_id),"managed view UUID was not reserved");
        let reserved=self.project.run(&fence.run_id)?.reservation(&fence.attempt_id)?.context("managed reservation absent")?;
        ensure!(matches!(reserved,ManagedRunSource::Reserved { fence:ref actual,runnable_root:ref selected }
            if actual==fence && selected==root),"managed cache differs from durable reservation");
        // Validate actual bounded CAS metadata before lookup, including env and
        // profile/policy. The snapshot is derived here, never caller-supplied.
        root.validate()?; ensure!(root.kind==ObjectKind::RunnableRoot,"managed cache requires RunnableRoot");
        let runnable=decode_runnable(&self.store.read_object(root)?)?;
        ensure!(&runnable.source==source,"managed seed source differs from runnable selection");
        let decoded_source=decode_source(&self.store.read_object(source)?)?;
        if let Some(environment)=&runnable.environment { decode_environment(&self.store.read_object(environment)?)?; }
        let profile:WorkspaceProfile=serde_json::from_slice(&self.store.read_object(&runnable.profile)?)?;
        profile.validate()?;
        let policy=SourceProjectionPolicy::from_import_policy(&profile.source_exclusions())?;
        ensure!(self.store.read_object(&decoded_source.projection_policy)?==canonical_json(&policy)?,
            "managed source projection differs from selected profile");
        let snapshot=VfsCheckpoint { format_version:1,directory:runnable.view_root.clone(),hardlinks:runnable.hardlinks.clone() };
        let key=Key { cache_format:CACHE_FORMAT,paged_format:PAGED_VIEW_FORMAT,root:root.clone(),runnable,snapshot };
        let started=Instant::now(); let mut state=self.lock(cancelled,budget)?; let wait_ns=started.elapsed().as_nanos();
        Self::time(cancelled,budget)?; self.custody()?; fence.check_current(journal)?;
        // Atomic pin_existing keeps GC authority; a matching digest/path or
        // durable index alone can never turn an unretained root into a hit.
        self.before_effect(cancelled,budget)?;
        if let Some(backed)=&backed_inputs{backed.check_for_new_task(budget,cancelled)?;backed.matches(&self.store,root,source)?;}
        else{self.store.pin_existing(&format!("managed:preparation-cache:{}:{}",self.lease.owner().epoch,root.digest),root)?;}
        self.before_effect(cancelled,budget)?;
        let hit=state.ready.iter().find(|floor|floor.key==key).cloned();if let Some(floor)=&hit{if let Some(backed)=&floor.backed_inputs{backed.check_retained()?;}}
        let prep_start=Instant::now();
        let floor=if let Some(floor)=&hit { floor.clone() } else {
            state.preparations=state.preparations.checked_add(1).context("preparation counter overflow")?;
            self.manager.prepare_paged_snapshot(&key.snapshot)?;
            Floor { metadata_bytes:key.snapshot.directory.size_bytes.checked_add(key.snapshot.hardlinks.size_bytes)
                .context("preparation metadata size overflow")?,key,backed_inputs:backed_inputs.clone() }
        };
        let prepare_ns=if hit.is_some() { 0 } else { prep_start.elapsed().as_nanos() };
        Self::time(cancelled,budget)?; self.custody()?; fence.check_current(journal)?;
        // Existing fork API is idempotent. Cache admission explicitly requires
        // a fresh writable view, so repeated IDs never reopen dirty state.
        for path in [self.manager.root().join("views").join(view_id),
            self.manager.root().join("requests").join(format!("{view_id}.json"))] {
            match fs::symlink_metadata(&path) {
                Ok(_) => anyhow::bail!("managed cache view UUID already used; recovery required"),
                Err(error) if error.kind()==std::io::ErrorKind::NotFound => {},
                Err(error) => return Err(error.into()),
            }
        }
        self.before_effect(cancelled,budget)?;
        let fork_start=Instant::now();
        let view=self.manager.fork_paged_snapshot(view_id,&floor.key.snapshot,fence.owner_epoch())?;
        let fork_ns=fork_start.elapsed().as_nanos();
        Self::time(cancelled,budget)?; self.custody()?; fence.check_current(journal)?;
        self.before_effect(cancelled,budget)?;
        // Commit a hit only after the entire floor/fresh-view operation succeeds.
        // A timed-out/failed preparation leaves durable recovery data, no hit.
        if hit.is_none() {
            if state.ready.len()==MAX_FLOORS { state.ready.remove(0); }
            state.ready.push(floor.clone()); self.ready.store(state.ready.len(),Ordering::Release);
        }
        let stats=ManagedPreparationReuse { cache_hit:hit.is_some(),preparation_calls:state.preparations,
            cached_floors:state.ready.len(),reused_floor_metadata_bytes:if hit.is_some(){floor.metadata_bytes}else{0},
            wait_ns,prepare_ns,fork_ns };
        drop(state);
        // The local admission drops only after this actual synchronous call
        // settles. Every failure unwinds its later-created view/IO guards first.
        // Live view/owner custody continues independently of preparation slots.
        Ok(ManagedPreparedView { view,cache:Arc::clone(self),stats })
    }
    /// Pure telemetry: active synchronous preparations and ready in-memory keys.
    pub(crate) fn counts(&self) -> (usize,usize) {
        (self.active.load(Ordering::Acquire),self.ready.load(Ordering::Acquire))
    }
}

#[cfg(test)]
#[path="managed_preparation_cache_tests.rs"]
mod tests;
