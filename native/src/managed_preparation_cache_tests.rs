use super::*;
use crate::engine_host::{EngineOwnership,NativeAuthority};
use rudder_engine::{journal::ManagedProjectPrepare, protocol::CheckpointConsistency,
    snapshot::{EnvironmentV2,SourceV2},vfs::HardlinkTopology};
use std::{collections::BTreeMap,os::unix::fs::PermissionsExt,sync::mpsc,thread};

#[path = "managed_preparation_cache_qualification.rs"]
mod qualification;

struct Temporary(PathBuf);
impl Temporary {
    fn new() -> Self {
        let path=std::env::temp_dir().join(format!("rudder-preparation-reuse-{}",uuid::Uuid::new_v4()));
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap(); Self(path.canonicalize().unwrap())
    }
}
impl Drop for Temporary { fn drop(&mut self) { let _=fs::remove_dir_all(&self.0); } }
struct Fixture {
    cache:Arc<ManagedPreparationCache>,journal:Journal,owner:EngineOwnership,
    project:ManagedStore,store:LocalStore,temporary:Temporary,
}
impl Fixture {
    fn new() -> Self {
        let temporary=Temporary::new(); let repo=temporary.0.join("repo"); let home=temporary.0.join("home");
        for path in [&repo,&home] { fs::DirBuilder::new().mode(0o700).create(path).unwrap(); }
        let mut owner=EngineOwnership::acquire_in_home(&repo,NativeAuthority::Fabric,&home).unwrap();
        let fence=owner.journal_parts().1.clone();
        let namespace=crate::managed_store::namespace(&home,&repo,&fence.project_id,owner.journal_parts().0).unwrap();
        let project=owner.prepare_managed_project(&ManagedProjectPrepare { request_id:uuid::Uuid::new_v4().to_string(),namespace }).unwrap();
        let journal=owner.source_journal_factory(&fence).unwrap()().unwrap();
        let store=LocalStore::open(temporary.0.join("cas")).unwrap();
        let cache=ManagedPreparationCache::open(owner.source_lease(&fence).unwrap(),&journal,project.clone(),store.clone()).unwrap();
        Self { cache,journal,owner,project,store,temporary }
    }
    fn root(&self,contents:&[u8],environment:Option<&str>,bad_topology:bool) -> ObjectRef {
        let path=self.temporary.0.join(uuid::Uuid::new_v4().to_string());
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap(); fs::write(path.join("data"),contents).unwrap();
        fs::set_permissions(path.join("data"),fs::Permissions::from_mode(0o600)).unwrap();
        let directory=self.store.import_tree(&path,CheckpointConsistency::Quiesced).unwrap();
        let profile=WorkspaceProfile::source_only();
        let policy=SourceProjectionPolicy::from_import_policy(&profile.source_exclusions()).unwrap();
        let policy=self.store.put_blob(&canonical_json(&policy).unwrap()).unwrap();
        let source=self.store.put_metadata(ObjectKind::SourceRoot,&SourceV2 { format_version:2,
            consistency:CheckpointConsistency::Quiesced,directory:directory.clone(),projection_policy:policy }).unwrap();
        let profile=self.store.put_blob(&canonical_json(&profile).unwrap()).unwrap();
        let hardlinks=self.store.put_blob(&if bad_topology { b"{}".to_vec() }
            else { canonical_json(&HardlinkTopology { format_version:1,groups:vec![] }).unwrap() }).unwrap();
        let environment=environment.map(|abi|self.store.put_metadata(ObjectKind::EnvironmentRoot,&EnvironmentV2 {
            format_version:2,directory:directory.clone(),platform:BTreeMap::from([("fixture_abi".into(),abi.into())]),
            installer_inputs:vec![],provenance:BTreeMap::new() }).unwrap());
        let root=self.store.put_metadata(ObjectKind::RunnableRoot,&RunnableRoot { format_version:1,
            consistency:CheckpointConsistency::Quiesced,source,environment,view_root:directory,profile,hardlinks,knowledge_cursor:0 }).unwrap();
        self.store.pin(&format!("fixture:{}",root.digest),&root).unwrap(); root
    }
    fn reserve(&mut self,root:&ObjectRef,run:&str) -> ManagedLaunchFence {
        let attempt=uuid::Uuid::new_v4().to_string(); let owner=self.cache.lease.owner().clone();
        let run_epoch=self.journal.observe_run_attempt(&owner,run,&attempt,1).unwrap();
        let fence=ManagedLaunchFence { owner,run_id:run.into(),run_epoch,attempt_id:attempt,request_id:uuid::Uuid::new_v4().to_string() };
        self.project.run(run).unwrap().persist_reservation(&ManagedRunSource::Reserved { fence:fence.clone(),runnable_root:root.clone() }).unwrap(); fence
    }
    fn fork(&mut self,root:&ObjectRef,run:&str) -> Result<ManagedPreparedView> {
        let fence=self.reserve(root,run); let source=decode_runnable(&self.store.read_object(root)? )?.source;
        self.cache.fork_selected(&fence,root,&source,&uuid::Uuid::new_v4().to_string(),&self.journal,
            &AtomicBool::new(false),&FinalizationBudget::begin(Duration::from_secs(10))?)
    }
}

#[test]
fn second_attempt_reuses_exact_floor_but_writable_views_and_namespaces_are_private() {
    let mut f=Fixture::new(); let root=f.root(b"original",Some("abi-a"),false);
    let first=f.fork(&root,"first").unwrap(); let second=f.fork(&root,"second").unwrap();
    assert!(!first.stats().cache_hit); assert!(second.stats().cache_hit);
    assert_eq!(first.stats().preparation_calls,1); assert_eq!(second.stats().preparation_calls,1);
    assert!(second.stats().reused_floor_metadata_bytes>0); assert_eq!(second.stats().prepare_ns,0);
    assert_eq!(first.manager().root(),second.manager().root()); assert_ne!(first.view().id(),second.view().id());
    assert_ne!(first.view().dirty_cursor().unwrap().namespace,second.view().dirty_cursor().unwrap().namespace);
    let a=first.view().lookup(1,b"data").unwrap(); let b=second.view().lookup(1,b"data").unwrap();
    first.view().write(a.ino,0,b"changed!",true).unwrap();
    assert_eq!(first.view().read(a.ino,0,8).unwrap(),b"changed!");
    assert_eq!(second.view().read(b.ino,0,8).unwrap(),b"original");
    let third=f.fork(&root,"third-live").unwrap(); assert!(third.stats().cache_hit);
    let c=third.view().lookup(1,b"data").unwrap(); assert_eq!(third.view().read(c.ino,0,8).unwrap(),b"original");
    assert_ne!(third.view().id(),first.view().id()); assert_ne!(third.view().id(),second.view().id());
    assert_eq!(f.cache.counts(),(0,1),"three live views retain custody without occupying preparation slots");
    drop(first); assert_eq!(f.cache.counts(),(0,1));
}

#[test]
fn preparation_caller_rejects_source_that_is_not_the_sealed_view_complement() {
    let mut f=Fixture::new();
    let selected=f.root(b"selected source",None,false);
    let other=f.root(b"different source",None,false);
    let mut forged=decode_runnable(&f.store.read_object(&selected).unwrap()).unwrap();
    forged.source=decode_runnable(&f.store.read_object(&other).unwrap()).unwrap().source;
    let forged=f.store.put_metadata(ObjectKind::RunnableRoot,&forged).unwrap();
    f.store.pin("forged-complement-test",&forged).unwrap();
    assert!(f.fork(&forged,"forged-source-complement").is_err());
    assert_eq!(f.cache.counts(),(0,0));
    let good=f.fork(&selected,"valid-after-refusal").unwrap();
    assert!(!good.stats().cache_hit);
}

#[test]
fn source_snapshot_environment_and_format_changes_never_borrow_an_old_hit() {
    let mut f=Fixture::new(); let a=f.root(b"a",Some("abi-a"),false);
    let same=f.root(b"a",Some("abi-a"),false); assert_eq!(a,same);
    drop(f.fork(&a,"a").unwrap());
    let b=f.root(b"b",Some("abi-a"),false); let prepared=f.fork(&b,"b").unwrap(); assert!(!prepared.stats().cache_hit); drop(prepared);
    let c=f.root(b"b",Some("abi-b"),false); let prepared=f.fork(&c,"c").unwrap(); assert!(!prepared.stats().cache_hit);
    assert_eq!(prepared.stats().cached_floors,2); drop(prepared);
    let prepared=f.fork(&a,"evicted").unwrap(); assert!(!prepared.stats().cache_hit);
    assert_eq!(prepared.stats().preparation_calls,4); drop(prepared);
    let mut invalid=decode_runnable(&f.store.read_object(&a).unwrap()).unwrap(); invalid.format_version=2;
    assert!(f.store.put_metadata(ObjectKind::RunnableRoot,&invalid).is_err(),"CAS refuses an unsupported root schema before cache admission");
    assert_eq!(f.cache.counts().0,0);
}

#[test]
fn failed_preparation_never_installs_a_hit_and_duplicate_view_ids_never_reopen_dirty_state() {
    let mut f=Fixture::new(); let bad=f.root(b"bad",None,true);
    assert!(f.fork(&bad,"bad-one").is_err()); assert!(f.fork(&bad,"bad-two").is_err());
    assert_eq!(f.cache.counts(),(0,0)); assert_eq!(f.cache.state.lock().unwrap().preparations,2);
    let good=f.root(b"good",None,false); let fence=f.reserve(&good,"good"); let view=uuid::Uuid::new_v4().to_string();
    let source=decode_runnable(&f.store.read_object(&good).unwrap()).unwrap().source;
    let budget=FinalizationBudget::begin(Duration::from_secs(10)).unwrap(); let cancelled=AtomicBool::new(false);
    let prepared=f.cache.fork_selected(&fence,&good,&source,&view,&f.journal,&cancelled,&budget).unwrap();
    assert!(f.cache.fork_selected(&fence,&good,&source,&view,&f.journal,&cancelled,&budget).is_err());
    assert_eq!(f.cache.counts().0,0); drop(prepared); assert_eq!(f.cache.counts().0,0);
}

#[test]
fn concurrent_misses_coalesce_with_two_preparation_slots_and_independent_live_view_custody() {
    let mut f=Fixture::new(); let root=f.root(b"shared",None,false);
    let fences=[f.reserve(&root,"left"),f.reserve(&root,"right")];
    let busy_fence=f.reserve(&root,"busy");
    let source=decode_runnable(&f.store.read_object(&root).unwrap()).unwrap().source;
    let guard=f.cache.state.lock().unwrap(); let mut threads=Vec::new();
    for fence in fences {
        let cache=Arc::clone(&f.cache); let writer=f.owner.source_journal_factory(f.cache.lease.owner()).unwrap();
        let root=root.clone(); let source=source.clone();
        let journal=writer().unwrap(); let budget=FinalizationBudget::begin(Duration::from_secs(10)).unwrap();
        threads.push(thread::spawn(move || {
            cache.fork_selected(&fence,&root,&source,&uuid::Uuid::new_v4().to_string(),&journal,&AtomicBool::new(false),&budget).unwrap() }));
    }
    let until=Instant::now()+Duration::from_secs(2);
    while f.cache.counts().0<2 && Instant::now()<until { thread::sleep(Duration::from_millis(1)); }
    let both_admitted=f.cache.counts().0==2;
    let busy=f.cache.fork_selected(&busy_fence,&root,&source,&uuid::Uuid::new_v4().to_string(),&f.journal,
        &AtomicBool::new(false),&FinalizationBudget::begin(Duration::from_millis(20)).unwrap());
    drop(guard);
    let views=threads.into_iter().map(|thread|thread.join().unwrap()).collect::<Vec<_>>();
    assert!(both_admitted);
    assert_eq!(views.iter().filter(|view|view.stats().cache_hit).count(),1);
    assert!(views.iter().all(|view|view.stats().preparation_calls==1)); assert_eq!(f.cache.counts(),(0,1));
    match busy { Ok(_) => panic!("two actual preparations must bound admission"),
        Err(error) => assert!(error.to_string().contains("admission is busy")) }
    let third=f.fork(&root,"third-after-settlement").unwrap(); assert!(third.stats().cache_hit);
    assert!(views.iter().all(|view|view.view().id()!=third.view().id()));
    assert_eq!(f.cache.counts(),(0,1)); drop(views); drop(third); assert_eq!(f.cache.counts(),(0,1));
}

#[test]
fn mutex_wait_expires_under_original_budget_without_releasing_unsettled_other_work() {
    let mut f=Fixture::new(); let root=f.root(b"wait",None,false); let fence=f.reserve(&root,"wait");
    let source=decode_runnable(&f.store.read_object(&root).unwrap()).unwrap().source;
    let writer=f.owner.source_journal_factory(f.cache.lease.owner()).unwrap(); let journal=writer().unwrap(); let cache=Arc::clone(&f.cache);
    let guard=f.cache.state.lock().unwrap(); let budget=FinalizationBudget::begin(Duration::from_millis(20)).unwrap();
    let (send,receive)=mpsc::sync_channel(1);
    let worker=thread::spawn(move || {
        let result=cache.fork_selected(&fence,&root,&source,&uuid::Uuid::new_v4().to_string(),&journal,&AtomicBool::new(false),&budget);
        let _=send.send(result.is_err()); result });
    let settled=receive.recv_timeout(Duration::from_secs(2)); drop(guard);
    let result=worker.join().unwrap(); assert_eq!(settled.unwrap(),true); assert!(result.is_err());
    assert_eq!(f.cache.counts(),(0,0));
}

#[test]
fn cancelled_stale_and_changed_cas_custody_are_not_preparation_hits() {
    let mut f=Fixture::new(); let root=f.root(b"owned",None,false); let fence=f.reserve(&root,"owned");
    let source=decode_runnable(&f.store.read_object(&root).unwrap()).unwrap().source;
    let cancelled=AtomicBool::new(true); let budget=FinalizationBudget::begin(Duration::from_secs(10)).unwrap();
    assert!(f.cache.fork_selected(&fence,&root,&source,&uuid::Uuid::new_v4().to_string(),&f.journal,&cancelled,&budget).is_err());
    cancelled.store(false,Ordering::Release);
    f.journal.observe_run_attempt(&fence.owner,&fence.run_id,&uuid::Uuid::new_v4().to_string(),2).unwrap();
    assert!(f.cache.fork_selected(&fence,&root,&source,&uuid::Uuid::new_v4().to_string(),&f.journal,&cancelled,&budget).is_err());
    let old=f.store.root().join("objects-held"); fs::rename(f.store.root().join("objects"),&old).unwrap();
    fs::DirBuilder::new().mode(0o700).create(f.store.root().join("objects")).unwrap();
    assert!(f.cache.custody().is_err()); assert!(f.cache.fenced.load(Ordering::Acquire));
    assert_eq!(f.cache.counts(),(0,0));
}
