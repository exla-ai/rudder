//! Bounded policy DATA selection from an already verified accepted source.
//! Only the adjacent original acceptor/SDK producer can issue gate evidence.
use super::*;
use rudder_engine::{build::BuildProfile,content::read_file_range,directory::EntryNode,tree};
use std::collections::{BTreeMap,BTreeSet};
const MAX_POLICY:u64=128*1024;
#[derive(serde::Deserialize)]#[serde(deny_unknown_fields)]
struct PolicyChange{format_version:u32,schema:String,expected_head:String,previous_profile:ObjectRef,next_profile:ObjectRef}
struct PolicyFile{reference:ObjectRef,bytes:Vec<u8>,profile:BuildProfile}
fn policy_file(store:&LocalStore,directory:&ObjectRef,original:&FinalizationBudget,cancel:&AtomicBool)->Result<PolicyFile>{
    time(original,cancel)?;let entry=tree::lookup(store,directory,b"rudder.toml")?.context("accepted/candidate rudder.toml absent; trusted initial build policy required")?;
    let EntryNode::File{object,logical_size,..}=entry.node else{anyhow::bail!("canonical rudder.toml must be a regular immutable source file")};
    ensure!(logical_size>0&&logical_size<=MAX_POLICY,"canonical policy byte bound");let bytes=read_file_range(store,&object,0,usize::try_from(logical_size)?)?;
    ensure!(bytes.len()as u64==logical_size,"canonical policy incomplete");let profile=BuildProfile::from_toml(std::str::from_utf8(&bytes)?)?;time(original,cancel)?;
    Ok(PolicyFile{reference:ObjectRef::from_bytes(ObjectKind::Blob,&bytes),bytes,profile})
}
fn select_profiles(base:&PolicyFile,next:&PolicyFile,snapshot:&rudder_engine::journal::CanonicalDecisionSnapshot)->Result<Vec<BuildProfile>>{
    if base.profile==next.profile{return Ok(vec![base.profile.clone()])}
    let accepted=snapshot.policy_decision_data().context("changed candidate build policy requires actual accepted build-policy/canonical decision")?;
    ensure!(accepted.project_id==snapshot.owner_data().project_id&&accepted.topic==rudder_engine::journal::CANONICAL_BUILD_POLICY_TOPIC&&accepted.scope=="build-policy","canonical policy decision scope differs");
    let change:PolicyChange=serde_json::from_value(accepted.statement.clone())?;
    ensure!(change.format_version==1&&change.schema=="canonical-gates-policy-v1"&&change.expected_head==snapshot.head_data()
        &&change.previous_profile==base.reference&&change.next_profile==next.reference,"accepted policy change does not bind this base/candidate policy");
    // A candidate cannot remove this batch's accepted gates. Explicitly
    // authorized policy changes execute both the old and proposed profiles.
    Ok(vec![base.profile.clone(),next.profile.clone()])
}
#[derive(Clone,PartialEq,Eq)]struct FileData{content:ObjectRef,mode:u32,logical:u64}
fn source_files(store:&LocalStore,root:&ObjectRef,original:&FinalizationBudget,cancel:&AtomicBool)->Result<BTreeMap<String,FileData>>{
    let mut out=BTreeMap::new();let mut todo=vec![(root.clone(),String::new(),0usize)];let mut entries=0usize;let mut names=0usize;
    while let Some((dir,prefix,depth))=todo.pop(){time(original,cancel)?;ensure!(depth<=256,"canonical source path depth");let mut after=None;
        loop{time(original,cancel)?;let page=tree::scan(store,&dir,after.as_deref(),1024)?;for entry in page.entries{entries+=1;ensure!(entries<=100_000,"canonical source entry bound");
            let raw=entry.name()?;let name=std::str::from_utf8(&raw)?;let path=if prefix.is_empty(){name.into()}else{format!("{prefix}/{name}")};
            names=names.checked_add(path.len()).context("canonical source names overflow")?;ensure!(names<=32*1024*1024&&path.len()<=4096,"canonical source name work bound");
            match entry.node{EntryNode::Directory{object}=>todo.push((object,path,depth+1)),EntryNode::File{object,logical_size,mode}=>{ensure!(out.insert(path,FileData{content:object,mode,logical:logical_size}).is_none(),"canonical source duplicate path")},EntryNode::Symlink{..}=>anyhow::bail!("A2 canonical compiler refuses source symlinks")}
        }after=page.next_after;if after.is_none(){break;}}
    }time(original,cancel)?;Ok(out)
}
fn code_path(path:&str)->bool{path.rsplit('.').next().is_some_and(|e|matches!(e,"rs"|"ts"|"tsx"|"mts"|"cts"|"js"|"mjs"|"cjs"|"c"|"cc"|"cpp"|"cxx"|"h"|"hpp"|"py"|"go"|"java"|"kt"|"swift"|"sh"))}
fn changed_sources_covered(base:&BTreeMap<String,FileData>,candidate:&BTreeMap<String,FileData>,profiles:&[BuildProfile])->Result<()> {
    let declared:BTreeSet<_>=profiles.iter().flat_map(|p|p.targets.iter().flat_map(|t|t.sources.iter().cloned())).collect();
    for path in base.keys().chain(candidate.keys()){if code_path(path)&&base.get(path)!=candidate.get(path){ensure!(declared.contains(path),"changed source is outside accepted/authorized canonical targets: {path}");}}Ok(())
}
pub(super) struct RetainedCanonicalPolicy{pub(super) snapshot:Option<Arc<rudder_engine::journal::CanonicalDecisionSnapshot>>,base_verified:Option<Arc<VerifiedSourceProjection>>,base:Option<PolicyFile>,next:Option<PolicyFile>,
    pub(super) profiles:Vec<BuildProfile>,stage:Option<String>,pin:String,pub(super) record:Option<ObjectRef>,pin_attempted:bool,pub(super) ready:bool,attempted:bool}
impl RetainedCanonicalPolicy{
    pub(super) fn staged()->Self{Self{snapshot:None,base_verified:None,base:None,next:None,profiles:Vec::new(),stage:None,pin:format!("canonical-policy:{}",uuid::Uuid::new_v4()),record:None,pin_attempted:false,ready:false,attempted:false}}
    pub(super) fn capture(&mut self,acceptor:&ManagedCheckpointAcceptance,journal:&Journal,original:&FinalizationBudget,cancel:&Arc<AtomicBool>)->Result<()> {
        ensure!(!self.attempted,"original canonical policy cannot retry");self.attempted=true;time(original,cancel)?;acceptor.check(journal)?;
        let binding=acceptor.binding.as_ref().context("actual candidate binding absent")?;let candidate=acceptor.candidate.as_ref().context("actual candidate absent")?;
        self.snapshot=Some(Arc::new(journal.capture_canonical_decisions(acceptor.lease.owner(),&binding.baseline.commit.0,original,cancel)?));
        let base_source=acceptor.completion.assignment().source.clone();let source=snapshot::decode_source(&acceptor.store.read_object(&base_source)?)?;
        let repository=acceptor.repository.as_ref().context("actual integration repository absent")?;repository.check_current()?;
        // Re-verify against the actual accepted jj commit, never filesystem HEAD
        // or candidate policy. The proof and staging owner are retained first.
        let heads=pollster::block_on(repository.loader.op_heads_store().get_op_heads())?;ensure!(heads.len()==1,"canonical accepted operation heads diverged");
        let operation=pollster::block_on(repository.loader.load_operation(&heads[0]))?;let repo=pollster::block_on(repository.loader.load_at(&operation))?;
        let commit=repo.store().get_commit(&CommitId::new(decode_hex(&binding.baseline.commit.0)?))?;ensure!(CommitIdentity::from_jj(&commit)==binding.baseline,"canonical baseline commit changed");
        self.stage=Some(acceptor.store.begin_staging(Duration::from_secs(120))?.id);
        if let Some(actual)=&acceptor.completion.prepared().backed_inputs{actual.check_for_new_task(original,cancel)?;actual.matches(&acceptor.store,&acceptor.completion.assignment().runnable_root,&base_source)?;
            let verified=actual.verified_source();ensure!(verified.receipt().source==base_source&&verified.receipt().projection_policy==binding.projection_policy&&verified.receipt().tree==binding.baseline.tree,"original accepted mapping differs from canonical base");self.base_verified=Some(verified.clone());
        }else{let mut projection=ProjectionBudget::default();projection.max_elapsed_ms=u64::try_from(original.remaining()?.as_millis().min(30_000))?;
            self.base_verified=Some(Arc::new(pollster::block_on(verify_versioned_source(acceptor.store.clone(),self.stage.as_ref().unwrap(),&base_source,&binding.projection_policy,&commit.tree(),None,projection))?));}
        ensure!(self.base_verified.as_ref().unwrap().receipt().tree==binding.baseline.tree,"accepted source mapping changed");time(original,cancel)?;
        self.base=Some(policy_file(&acceptor.store,&source.directory,original,cancel)?);self.next=Some(policy_file(&acceptor.store,&candidate.directory,original,cancel)?);
        self.profiles=select_profiles(self.base.as_ref().unwrap(),self.next.as_ref().unwrap(),self.snapshot.as_ref().unwrap())?;
        let base=source_files(&acceptor.store,&source.directory,original,cancel)?;let next=source_files(&acceptor.store,&candidate.directory,original,cancel)?;changed_sources_covered(&base,&next,&self.profiles)?;
        let stage=self.stage.as_ref().unwrap();for file in [&self.base,&self.next]{let file=file.as_ref().unwrap();let actual=acceptor.store.put_blob(&file.bytes)?;ensure!(actual==file.reference,"canonical policy CAS bytes differ");}
        let data=serde_json::json!({"schema":"canonical-policy-selection-v1","base_head":binding.baseline.commit,"base_source":base_source,
            "accepted_profile":self.base.as_ref().unwrap().reference,"candidate_profile":self.next.as_ref().unwrap().reference,"versions":self.snapshot.as_ref().unwrap().versions_data()});
        self.record=Some(acceptor.store.put_metadata(ObjectKind::ActionResult,&rudder_engine::store::ReferencedMetadata{format_version:1,references:vec![self.base.as_ref().unwrap().reference.clone(),self.next.as_ref().unwrap().reference.clone()],payload:data})?);
        self.pin_attempted=true;acceptor.store.publish(stage,&self.pin,self.record.as_ref().unwrap())?;
        self.snapshot.as_ref().unwrap().assert_current(journal,original,cancel)?;acceptor.check(journal)?;time(original,cancel)?;self.ready=true;Ok(())
    }
}
#[cfg(test)]
#[path="managed_canonical_policy_tests.rs"]mod tests;
