//! Explicit large source-producer COMPONENT fixture, not Root/model qualification.
//! All mutable files/repositories belong to the fresh /workspace run directory.
//! Construction is separate from original30s production phase measurements.
use super::*;
use anyhow::{Context,Result,ensure};
use serde_json::{Value,json};
use sha2::{Digest,Sha256};
use std::{io::Write,time::Instant,os::unix::fs::PermissionsExt};
use rudder_engine::{snapshot::{EnvironmentV2,SourceV2,RunnableRoot},protocol::CheckpointConsistency,
    tree::{ImportBudget,import_quiesced},workspace::WorkspaceProfile};
use crate::{initial_source::{PreparedInitialSource,InitialSourceRequest,prepare_initial_source_profile_with_import_retaining,AcceptedSourceGeneration},
    projection::{materialize_source,verify_versioned_source},projection_io::ProjectionWork};
const INPUT_ROOT:&str="/workspace/rudder-directory-custody-image-check";
const INPUT_MANIFEST:&str="/workspace/rudder-directory-custody-image-source-manifest.json";
const INPUT_SHA:&str="30d1ac637796c0da5940b5872f74b52e7343c3d85b193bcb4daab9d3d17148c6";
const INPUT_FILES:usize=2011;
const INPUT_BYTES:u64=58_466_942;
const HISTORY_COHORTS:usize=4;
const HISTORY_FILES:usize=3200;
const HISTORY_FILE_BYTES:usize=9<<10;
const EDIT:&str="src/main.ts";

struct Corpus {root:PathBuf,path:PathBuf,frozen:PathBuf,history:PathBuf,store:LocalStore,settings:UserSettings,
    repo:Arc<jj_lib::repo::ReadonlyRepo>,record:Option<ProjectionRecord>,environment:ObjectRef,policy:ObjectRef,
    expected_head:crate::GitCommitId,history_oids:Vec<String>}
// No automatic directory deletion, history unpin, replay or cleanup-on-failure.
#[derive(Default)]
struct Capsule {cold_import:Option<crate::accepted_git_import::RetainedGitImport>,corpus:Option<Corpus>,cold_seed:Option<PagedSeedGeneration>,cold:Option<PreparedInitialSource>,
    accepted:Option<AcceptedSourceGeneration>,next:Option<PreparedInitialSource>,warm:Option<PagedSeedGeneration>,views:Vec<VfsView>}
fn make_dir(path:&Path)->Result<()>{fs::DirBuilder::new().mode(0o700).create(path)?;Ok(())}
fn remaining(original:&FinalizationBudget)->Result<ProjectionBudget>{Ok(ProjectionBudget{max_elapsed_ms:original.remaining()?.as_millis().min(30_000).try_into()?,..ProjectionBudget::default()})}
fn seed_counts(w:crate::seed::SeedCasWork)->Value{json!({"read_files":w.read_files,"read_bytes":w.read_bytes,"reused_files":w.reused_files,"scope":"actual-seed-producer-counters-not-physical-disk-cache"})}
fn paged_counts(w:PagedSeedWork)->Value{json!({"read_files":w.read_files,"read_bytes":w.read_bytes,"reused_files":w.reused_files,"scope":"actual-seed-producer-counters-not-physical-disk-cache"})}
fn measured<T>(report:&mut Value,label:&str,body:impl FnOnce(&mut Value)->Result<T>)->Result<T>{
    report["currentPhase"]=json!(label);let started=Instant::now();let result=body(report);
    report["phases"].as_array_mut().unwrap().push(json!({"name":label,"elapsed_ms":started.elapsed().as_secs_f64()*1000.0,"passed":result.is_ok(),"error":result.as_ref().err().map(|e|format!("{e:#}"))}));result
}
fn import(store:&LocalStore,path:&Path)->Result<ObjectRef>{
    let stage=store.begin_staging(Duration::from_secs(120))?;
    let tree=import_quiesced(store,&stage.id,path,&WorkspaceProfile::source_only().source_exclusions(),&ImportBudget::default())?;
    store.pin(&format!("qualification-topology:{}",uuid::Uuid::new_v4()),&tree.hardlinks)?;
    store.publish(&stage.id,&format!("qualification-tree:{}",uuid::Uuid::new_v4()),&tree.directory)?;Ok(tree.directory)
}
fn copy_frozen(root:&Path)->Result<PathBuf>{
    let bytes=fs::read(INPUT_MANIFEST)?;ensure!(hex::encode(Sha256::digest(&bytes))==INPUT_SHA,"dataset manifest changed");
    let manifest:Value=serde_json::from_slice(&bytes)?;ensure!(manifest["frozenRoot"]==INPUT_ROOT,"dataset root differs");
    let files=manifest["files"].as_object().context("frozen file map absent")?;ensure!(files.len()==INPUT_FILES,"frozen file count differs");
    let destination=root.join("frozen-source-input");make_dir(&destination)?;let mut total=0;
    for(name,hash)in files{let relative=Path::new(name);ensure!(!relative.is_absolute()&&relative.components().all(|c|matches!(c,std::path::Component::Normal(_))),"unsafe frozen path");
        let source=Path::new(INPUT_ROOT).join(relative);let metadata=fs::symlink_metadata(&source)?;ensure!(source.canonicalize()?==source&&metadata.is_file(),"dataset path redirected");
        let bytes=fs::read(&source)?;ensure!(bytes.len()<=32<<20&&hash.as_str()==Some(hex::encode(Sha256::digest(&bytes)).as_str()),"frozen bytes differ/exceed existing Git object bound");
        total+=bytes.len()as u64;let output=destination.join(relative);fs::create_dir_all(output.parent().unwrap())?;fs::write(&output,bytes)?;fs::set_permissions(output,fs::Permissions::from_mode(metadata.mode()&0o777))?;
    }ensure!(total==INPUT_BYTES,"frozen logical byte count differs");Ok(destination)
}
impl Corpus {
    fn capture(root:&Path)->Result<Self>{
        let frozen=copy_frozen(root)?;let history=root.join("history-input");make_dir(&history)?;let path=root.join("source-repository");make_dir(&path)?;
        let mut config=StackedConfig::with_defaults();let mut layer=ConfigLayer::empty(ConfigSource::User);layer.set_value("user.name","Owned real-size qualification")?;layer.set_value("user.email","fixture@invalid.example")?;config.add_layer(layer);
        let settings=UserSettings::from_config(config)?;let(_workspace,repo)=pollster::block_on(Workspace::init_internal_git(&settings,&path))?;
        let store=LocalStore::open(root.join("cas"))?;let empty=store.put_directory(DirectoryManifest{format_version:1,mode:0o755,entries:vec![]})?;
        let environment=store.put_metadata(ObjectKind::EnvironmentRoot,&EnvironmentV2{format_version:2,directory:empty,platform:BTreeMap::from([("qualification".into(),"inert-source-only".into())]),installer_inputs:vec![],provenance:BTreeMap::new()})?;store.pin("qualification-environment",&environment)?;
        let policy=store.put_blob(&canonical_json(&SourceProjectionPolicy::from_import_policy(&WorkspaceProfile::source_only().source_exclusions())?)?)?;
        let initial=repo.store().get_commit(repo.view().get_wc_commit_id(WorkspaceName::DEFAULT).unwrap())?;
        Ok(Self{root:root.into(),path,frozen,history,store,settings,expected_head:crate::GitCommitId(initial.id().hex()),repo,record:None,environment,policy,history_oids:Vec::new()})
    }
    fn commit(&mut self,directory:&ObjectRef)->Result<(ProjectionWork,Vec<String>)>{
        let parent=self.repo.store().get_commit(self.repo.view().get_wc_commit_id(WorkspaceName::DEFAULT).unwrap())?;
        // Setup commits are genuine bounded30s source projections, with the
        // actual last returned record; no source receipt or Git FileId is forged.
        let projection=pollster::block_on(project_source(self.store.clone(),directory,&parent.tree(),&self.policy,None,self.record.as_ref(),ProjectionBudget::default()))?;
        let mut tx=self.repo.start_transaction();let commit=pollster::block_on(tx.repo_mut().new_commit(vec![parent.id().clone()],projection.tree).write())?;
        tx.repo_mut().set_wc_commit(WorkspaceName::DEFAULT.to_owned(),commit.id().clone())?;tx.repo_mut().set_local_bookmark_target(RefName::new(INTEGRATION_BOOKMARK),RefTarget::normal(commit.id().clone()));
        self.repo=pollster::block_on(tx.commit("owned qualification history"))?;self.expected_head=crate::GitCommitId(commit.id().hex());
        let git=self.repo.store().backend_impl::<GitBackend>().unwrap().git_repo_path();fs::write(git.join("HEAD"),format!("{}\n",self.expected_head.0))?;
        let mut oids=Vec::new();for(_,value)in commit.tree().entries(){let value=value?;if let Some(Some(jj_lib::backend::TreeValue::File{id,..}))=value.as_resolved(){oids.push(id.hex());}}
        self.record=Some(projection.record);Ok((projection.work,oids))
    }
    fn history_cohort(&mut self,cohort:usize)->Result<ProjectionWork>{
        // Earlier cohort bytes remain reachable through real parent commits,
        // while the final HEAD contains only the exact frozen Rudder sources.
        for entry in fs::read_dir(&self.history)?{fs::remove_file(entry?.path())?;}
        for i in 0..HISTORY_FILES{let id=cohort*HISTORY_FILES+i;let mut state=(id as u64+1).wrapping_mul(0x9e3779b97f4a7c15);let mut bytes=vec![0;HISTORY_FILE_BYTES];
            for b in &mut bytes{state^=state<<13;state^=state>>7;state^=state<<17;*b=state as u8;}fs::write(self.history.join(format!("old-{id:05}.bin")),bytes)?;}
        let root=import(&self.store,&self.history)?;let(work,oids)=self.commit(&root)?;ensure!(oids.len()==HISTORY_FILES,"history tree lost actual blob entries");self.history_oids.extend(oids);Ok(work)
    }
}
fn object_inventory(path:&Path)->Result<Value>{let mut todo=vec![path.into()];let mut files=0u64;let mut bytes=0u64;
    while let Some(path)=todo.pop(){for entry in fs::read_dir(path)?{let entry=entry?;let m=fs::symlink_metadata(entry.path())?;ensure!(!m.file_type().is_symlink(),"owned Git objects contain a redirect");
        if m.is_dir(){todo.push(entry.path());}else{ensure!(m.is_file(),"owned Git object is special");files+=1;bytes+=m.len();ensure!(files<=100_000&&bytes<=384<<20,"fixture exceeds original Git profile");}}}Ok(json!({"files":files,"physical_bytes":bytes}))}
#[derive(Clone,Debug,PartialEq,Eq)]
struct BackingStamp {version:LooseBlobStamp,uid:u32,gid:u32,links:u64}
fn backing_stamp(path:&Path)->Result<BackingStamp>{let m=fs::symlink_metadata(path)?;ensure!(m.is_file()&&!m.file_type().is_symlink(),"actual Git backing is not a regular file");Ok(BackingStamp{version:loose_blob_stamp(path),uid:m.uid(),gid:m.gid(),links:m.nlink()})}
type BlobWitness=(String,PathBuf,Option<u64>,BackingStamp);
fn pack_indices(git:&Path)->Result<Vec<(PathBuf,gix::odb::pack::index::File)>>{
    let mut out=Vec::new();for entry in fs::read_dir(git.join("objects/pack"))?{let path=entry?.path();if path.extension().is_some_and(|v|v=="idx"){ensure!(out.len()<128,"fixture pack-count bound exceeded");out.push((path.clone(),gix::odb::pack::index::File::at(path,gix::hash::Kind::Sha1)?));}}out.sort_by(|a,b|a.0.cmp(&b.0));Ok(out)
}
fn witnesses(repo:&Arc<jj_lib::repo::ReadonlyRepo>)->Result<BTreeMap<String,BlobWitness>>{
    let commit=repo.store().get_commit(repo.view().get_wc_commit_id(WorkspaceName::DEFAULT).unwrap())?;let git=repo.store().backend_impl::<GitBackend>().unwrap().git_repo_path();let indices=pack_indices(git)?;let mut out=BTreeMap::new();
    for(path,value)in commit.tree().entries(){let value=value?;if let Some(Some(jj_lib::backend::TreeValue::File{id,..}))=value.as_resolved(){let oid=id.hex();let name=path.as_internal_file_string().to_owned();let physical=git.join("objects").join(&oid[..2]).join(&oid[2..]);
        let (physical,offset)=match fs::symlink_metadata(&physical){Ok(_)=>(physical,None),Err(e)if e.kind()==std::io::ErrorKind::NotFound=>{let id=gix::ObjectId::from_hex(oid.as_bytes())?;let matches=indices.iter().filter_map(|(p,index)|index.lookup(id).map(|i|(p.with_extension("pack"),index.pack_offset_at_index(i)))).collect::<Vec<_>>();ensure!(matches.len()==1,"actual FileId has missing/ambiguous packed backing");(matches[0].0.clone(),Some(matches[0].1))},Err(e)=>return Err(e.into())};
        let stamp=backing_stamp(&physical)?;out.insert(name,(oid,physical,offset,stamp));}}
    Ok(out)
}
fn sample(w:&BTreeMap<String,BlobWitness>)->Value{json!(w.iter().take(16).map(|(name,(id,path,offset,stamp))|{let s=&stamp.version;json!({"path":name,"actual_jj_file_id":id,"physical_backing":path,"pack_offset":offset,"backing_kind":if offset.is_some(){"shared immutable pack"}else{"loose object"},"logical_leaf_inode_claimed":false,"dev":s.dev,"ino":s.ino,"bytes":s.len,"mode":s.mode,"mtime":s.mtime,"ctime":s.ctime,"uid":stamp.uid,"gid":stamp.gid,"nlink":stamp.links})}).collect::<Vec<_>>())}
fn verified_packed_history(git:&Path)->Result<Value>{
    let clock=budget();let mut repository=gix::open(git)?;repository.object_cache_size(None);let mut ids=std::collections::BTreeSet::new();let mut bytes=0u64;let mut physical=0u64;let mut packs=0;
    for(path,index)in pack_indices(git)?{clock.remaining()?;packs+=1;let raw=fs::read(&path)?;ensure!(raw.len()>=40&&raw.len()<=64<<20,"actual Git index size out of bound");let mut hash=gix::hash::hasher(gix::hash::Kind::Sha1);hash.update(&raw[..raw.len()-20]);ensure!(hash.try_finalize()?.as_slice()==&raw[raw.len()-20..],"actual Git index checksum differs");
        let pack=path.with_extension("pack");let file=fs::File::open(&pack)?;let len=file.metadata()?.len();ensure!((32..=64<<20).contains(&len),"actual pack exceeds unchanged file quota");physical=physical.checked_add(len).context("pack bytes overflow")?;
        let mut hash=gix::hash::hasher(gix::hash::Kind::Sha1);let mut offset=0;let mut buf=[0;65536];while offset<len-20{clock.remaining()?;let n=usize::try_from((len-20-offset).min(65536))?;std::os::unix::fs::FileExt::read_exact_at(&file,&mut buf[..n],offset)?;hash.update(&buf[..n]);offset+=n as u64;}let sum=hash.try_finalize()?;let mut trailer=[0;20];std::os::unix::fs::FileExt::read_exact_at(&file,&mut trailer,len-20)?;ensure!(sum.as_slice()==&trailer&&sum==index.pack_checksum(),"actual pack checksum differs from verified index");
        for entry in index.iter(){clock.remaining()?;ensure!(ids.insert(entry.oid)&&ids.len()<=100_000,"duplicate/excess packed Git object");let header=repository.find_header(entry.oid)?;ensure!(header.size()<=32<<20&&header.size()<= (384u64<<20).saturating_sub(bytes),"actual Git history exceeds original profile");let object=repository.find_object(entry.oid)?;ensure!(object.kind==header.kind()&&object.data.len()as u64==header.size()&&gix::objs::compute_hash(gix::hash::Kind::Sha1,object.kind,&object.data)?==entry.oid,"actual packed Git preimage differs");bytes+=header.size();}
    }
    ensure!(packs>0&&ids.len()>=14_780&&bytes>=112<<20,"actual logical history is smaller than repository-sized gate");Ok(json!({"logical_git_objects":ids.len(),"verified_object_preimage_bytes":bytes,"physical_pack_bytes":physical,"packs":packs,"raw_object_ids_verified":true,"index_and_pack_checksums_verified":true,"scope":"actual target pack/index and object-reader verification; no physical-cache counter claim"}))
}

fn run(c:&mut Capsule,root:&Path,report:&mut Value)->Result<()> {
    c.corpus=Some(measured(report,"setup-copy-exact-frozen-source-and-owned-repository",|_phase_report|Corpus::capture(root))?);let f=c.corpus.as_mut().unwrap();
    for i in 0..HISTORY_COHORTS{let work=measured(report,&format!("setup-genuine-history-cohort-{i}"),|_phase_report|f.history_cohort(i))?;report["history_projection_work"].as_array_mut().unwrap().push(serde_json::to_value(work)?);}
    let frozen=measured(report,"setup-import-exact-frozen-source",|_phase_report|import(&f.store,&f.frozen))?;let(work,_)=measured(report,"setup-final-owned-frozen-HEAD",|_phase_report|f.commit(&frozen))?;report["frozen_projection_work"]=serde_json::to_value(work)?;
    ensure!(f.history_oids.len()==HISTORY_COHORTS*HISTORY_FILES,"history dataset incomplete");let git=f.repo.store().backend_impl::<GitBackend>().unwrap().git_repo_path().to_path_buf();report["owned_git_objects"]=object_inventory(&git.join("objects"))?;
    let private=root.join("imported-private-repository");make_dir(&private)?;let seed=root.join("absent-materialized-seed");
    let cold_clock=budget();let prepared=measured(report,"actual-initial-source-original30s",|_phase_report|{
        let value=pollster::block_on(prepare_initial_source_profile_with_import_retaining(f.store.clone(),InitialSourceRequest{source_git:&git,private_repository:&private,seed_destination:&seed,expected_head:&f.expected_head,environment:&f.environment,settings:&f.settings},
            SeedProfile::PagedCowV2,&mut c.cold_seed,&mut c.cold_import,&cold_clock,Arc::new(AtomicBool::new(false)),||Ok(())))?;cold_clock.remaining()?;Ok(value)
    })?;c.cold=Some(prepared);ensure!(!seed.exists(),"V2 unexpectedly materialized flat metadata");
    report["cold_seed_work"]=paged_counts(c.cold_seed.as_ref().unwrap().work());let old=c.cold.as_ref().unwrap();let old_seed=old.seed_root.clone();
    let(d,_,_)=measured(report,"inspect-actual-cold-seed-data",|_phase_report|read_paged_seed(&f.store,&old_seed,||Ok(())))?;
    report["cold_seed_input"]=json!({"metadata_files":d.metadata_files,"object_files":d.object_files,"metadata_bytes":d.metadata_bytes,"object_bytes":d.object_bytes});
    let private_git=old.repository().store().backend_impl::<GitBackend>().unwrap().git_repo_path();
    let actual_history=measured(report,"verify-actual-target-logical-history-and-pack-checksums",|_|verified_packed_history(private_git))?;report["actual_target_packed_history"]=actual_history;
    ensure!(d.object_bytes>=112<<20,"actual physical seed history bytes were smaller than repository-sized gate");
    report["cold_import_progress"]=serde_json::to_value(c.cold_import.as_ref().context("actual retained initial import absent")?.progress())?;
    let before=measured(report,"capture-actual-accepted-FileIds-and-inode-witnesses",|_phase_report|witnesses(old.repository()))?;report["actual_file_id_samples"]=sample(&before);
    let old_repo=old.repository().clone();let previous=old.baseline.clone();let cache=old.seed_cache().clone();let source=rudder_engine::snapshot::decode_source(&f.store.read_object(&old.source_root)?)?;
    // Obtain the genuine current baseline record from the public materializer.
    // This is separately measured preparation, not an invented equality hint.
    let stage=f.store.begin_staging(Duration::from_secs(120))?;let baseline_commit=old_repo.store().get_commit(old_repo.view().get_wc_commit_id(WorkspaceName::DEFAULT).unwrap())?;
    let(record,record_work)=measured(report,"actual-prior-record-materialization",|_phase_report|pollster::block_on(materialize_source(f.store.clone(),&stage.id,&baseline_commit.tree(),&source.projection_policy,None,ProjectionBudget::default())))?;
    ensure!(record.directory==source.directory,"actual prior record differs from original source");report["prior_record_work"]=serde_json::to_value(record_work)?;
    fs::OpenOptions::new().append(true).open(f.frozen.join(EDIT))?.write_all(b"\n// Owned repository-size source qualification edit.\n")?;
    let accepted_clock=budget();measured(report,"actual-accepted-source-original30s",|_phase_report|{
        accepted_clock.remaining()?;let directory=import(&f.store,&f.frozen)?;accepted_clock.remaining()?;
        let projected=pollster::block_on(project_source(f.store.clone(),&directory,&baseline_commit.tree(),&source.projection_policy,None,Some(&record),remaining(&accepted_clock)?))?;
        _phase_report["accepted_projection_work"]=serde_json::to_value(&projected.work)?;
        ensure!(projected.work.reused_leaf_values+1==before.len()as u64,"unchanged frozen leaves failed real previous-record reuse");
        let stage=f.store.begin_staging(Duration::from_secs(120))?;
        let source=f.store.put_metadata(ObjectKind::SourceRoot,&SourceV2{format_version:2,consistency:CheckpointConsistency::Quiesced,directory:directory.clone(),projection_policy:source.projection_policy.clone()})?;
        let verified=Arc::new(pollster::block_on(verify_versioned_source(f.store.clone(),&stage.id,&source,&record.projection_policy,&projected.tree,Some(&projected.record),remaining(&accepted_clock)?))?);
        let old_runnable=rudder_engine::snapshot::decode_runnable(&f.store.read_object(&old.runnable_root)?)?;
        let runnable=f.store.put_metadata(ObjectKind::RunnableRoot,&RunnableRoot{format_version:1,consistency:CheckpointConsistency::Quiesced,source:source.clone(),environment:Some(f.environment.clone()),view_root:directory,profile:old_runnable.profile,hardlinks:old_runnable.hardlinks,knowledge_cursor:0})?;
        f.store.publish_extending(&stage.id,"qualification-actual-accepted-candidate",&runnable,&[f.environment.clone()])?;
        let mut tx=old_repo.start_transaction();let commit=pollster::block_on(tx.repo_mut().new_commit(vec![baseline_commit.id().clone()],projected.tree).write())?;
        tx.repo_mut().set_wc_commit(WorkspaceName::DEFAULT.to_owned(),commit.id().clone())?;tx.repo_mut().set_local_bookmark_target(RefName::new(INTEGRATION_BOOKMARK),RefTarget::normal(commit.id().clone()));
        let repo=pollster::block_on(tx.commit("owned actual accepted generation"))?;accepted_clock.remaining()?;
        AcceptedSourceGeneration::prepare_retaining(&mut c.accepted,f.store.clone(),repo,verified,cache.clone(),source,runnable,f.environment.clone(),previous,old_seed.clone(),"qualification-next-runnable".into(),"qualification-next-seed".into(),accepted_clock.clone(),Arc::new(AtomicBool::new(false)),||Ok(()))?;
        c.accepted.as_mut().unwrap().take_prepared_retaining(&mut c.next)?;accepted_clock.remaining()?;Ok(())
    })?;
    report["accepted_seed_work"]=seed_counts(c.accepted.as_ref().unwrap().seed_work().context("accepted seed counters absent")?);
    let next=c.next.as_ref().unwrap();let after=witnesses(next.repository())?;let mut unchanged=0;
    for(name,witness)in &before{if name!=EDIT{ensure!(after.get(name)==Some(witness),"inherited actual Git FileId/inode/version changed: {name}");unchanged+=1;}}
    ensure!(after.get(EDIT).map(|v|&v.0)!=before.get(EDIT).map(|v|&v.0),"accepted edit did not change actual Git FileId");report["unchanged_git_file_id_and_stamp_witnesses"]=json!(unchanged);
    let warm_clock=budget();measured(report,"actual-unchanged-warm-generation-original30s",|_phase_report|{
        PagedSeedGeneration::prepare_retaining(&mut c.warm,cache,next.baseline.clone(),next.operation.clone(),"qualification-unchanged-warm".into(),Some(&next.seed_root),warm_clock.clone(),Arc::new(AtomicBool::new(false)),&mut||Ok(()))?;
        warm_clock.remaining()?;ensure!(c.warm.as_ref().unwrap().root()?==&next.seed_root&&c.warm.as_ref().unwrap().work().read_files==0,"unchanged warm generation reread content/changed root");Ok(())
    })?;report["warm_seed_work"]=paged_counts(c.warm.as_ref().unwrap().work());
    let(d,_,_)=read_paged_seed(&f.store,&next.seed_root,||Ok(()))?;
    let cow_clock=budget();measured(report,"two-logical-private-metadata-CoW-views-original30s",|_phase_report|{
        let manager=VfsManager::open(root.join("logical-cow"),f.store.clone())?;manager.prepare_paged(&d.private)?;
        for n in ["a","b"]{let id=uuid::Uuid::new_v4().to_string();let start=Instant::now();c.views.push(manager.fork_paged(&id,&d.private,OwnerEpoch{owner:"fixture-logical-cow".into(),epoch:1})?);_phase_report["forks"].as_array_mut().unwrap().push(json!({"id":id,"label":n,"elapsed_ms":start.elapsed().as_secs_f64()*1000.0}));cow_clock.remaining()?;}
        let a=&c.views[0];let b=&c.views[1];let ai=lookup(a,&[b".git",b"HEAD"]);let bi=lookup(b,&[b".git",b"HEAD"]);let original=b.read(bi,0,128)?;
        let handle=a.open_handle(ai,libc::O_RDWR,false)?;a.write(ai,0,b"PRIVATE-A\n",true)?;a.close_handle(ai,handle)?;ensure!(b.read(bi,0,128)?==original,"private A write crossed into B");
        let handle=b.open_handle(bi,libc::O_RDWR,false)?;b.write(bi,0,b"PRIVATE-B\n",true)?;b.close_handle(bi,handle)?;ensure!(a.read(ai,0,128)?.starts_with(b"PRIVATE-A\n")&&b.read(bi,0,128)?.starts_with(b"PRIVATE-B\n"),"two private writes lost isolation");
        ensure!(f.store.retention()?.pins.values().any(|r|r==&old_seed)&&f.store.retention()?.pins.values().any(|r|r==&next.seed_root),"original/current immutable seed pin lost");cow_clock.remaining()?;Ok(())
    })?;
    report["qualified"]=json!(true);report["currentPhase"]=json!("complete");Ok(())
}

#[test]
#[ignore="explicit large owned /workspace real Rudder corpus + reachable Git history; run --exact serially"]
fn frozen_rudder_repository_sized_initial_accepted_warm_and_private_cow(){
    let root=Path::new("/workspace").join(format!("rudder-real-repo-seed-{}",uuid::Uuid::new_v4()));make_dir(&root).unwrap();
    let mut capsule=Capsule::default();let mut report=json!({"formatVersion":1,"scope":"actual-local-source-producer-and-logical-CoW-component-only","qualified":false,"dataset_manifest_sha256":INPUT_SHA,"frozen_source_files":INPUT_FILES,"frozen_source_bytes":INPUT_BYTES,
        "generated_history_fixture":true,"requested_history_blobs":HISTORY_COHORTS*HISTORY_FILES,"requested_history_blob_bytes":HISTORY_COHORTS*HISTORY_FILES*HISTORY_FILE_BYTES,
        "phases":[],"history_projection_work":[],"forks":[],"original_production_phase_milliseconds":30000,"deadlinesRaised":false,"quotasWaived":false,"physicalRoot":false,"kernelFuse":false,"rootIpc":false,"nativeApp":false,"projectAuthority":false,"models":false,"physicalDiskCacheCounters":false,"normalCleanupClaimed":false,"filesRetainedForInspection":true});
    let total_started=Instant::now();let result=std::panic::catch_unwind(std::panic::AssertUnwindSafe(||run(&mut capsule,&root,&mut report)));
    report["total_elapsed_ms"]=json!(total_started.elapsed().as_secs_f64()*1000.0);
    if let Some(import)=&capsule.cold_import{report["last_actual_cold_import_progress"]=serde_json::to_value(import.progress()).unwrap();}
    if let Some(seed)=&capsule.cold_seed{report["last_actual_cold_seed_work"]=paged_counts(seed.work());}
    if let Some(accepted)=&capsule.accepted{report["last_actual_accepted_seed_work"]=accepted.seed_work().map(seed_counts).unwrap_or(Value::Null);}
    if let Some(warm)=&capsule.warm{report["last_actual_warm_seed_work"]=paged_counts(warm.work());}
    let ok=matches!(&result,Ok(Ok(())));report["qualified"]=json!(ok);if let Ok(Err(e))=&result{report["error"]=json!(format!("{e:#}"));}if result.is_err(){report["panic"]=json!(true);}
    fs::write(root.join("report.json"),serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    println!("RUDDER_REAL_REPO_SOURCE_COMPONENT_REPORT {}",root.join("report.json").display());assert!(ok,"actual source component failed; original owned files preserved at {}",root.display());
}
