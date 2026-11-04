//! OFF App selected-source compiler adapter. Only an actual original SDK
//! client can enter this path. SDK bytes stay on their separate Root FUSE
//! floor; only source is privately exported and outputs are locally published.
use super::*;
use rudder_engine::{build::{BuildInputView,BuildProfile},protocol::{ObjectKind,ObjectRef},store::{StagingLease,ReferencedMetadata}};
use std::{panic::{catch_unwind,AssertUnwindSafe},io::{Read,Write},os::unix::ffi::OsStringExt};
const MAX_SOURCE_FILES:usize=100_000;
const MAX_SOURCE_BYTES:u64=384*1024*1024;
const MAX_OUTPUT_BYTES:u64=128*1024*1024;

#[path="compiler_sdk_build_settlement.rs"]
mod settlement;
#[path="compiler_sdk_build_failure.rs"]mod terminal;
pub use terminal::CompilerSdkBuildFailure;
pub use settlement::{CompilerSdkJobDisposal,CompilerSdkJobSettled};
use super::super::sdk_job_protocol::{self as job_protocol,JobBinding};
struct NeverDispatched;
struct OriginalSdkExecution{binding:JobBinding,helper_pid:u32,helper_start:String,ready:bool}

pub struct RetainedCompilerSdkBuild {state:Arc<Mutex<BuildState>>,outcome:Option<CompilerSdkBuildOutcome>,failure:Option<CompilerSdkBuildFailure>}
/// Not Clone/Serde and has no public DATA constructor. Success requires the
/// genuine Root child result, original private pipe settlement and known CAS
/// publication; an old process report or matching key cannot construct it.
pub struct CompilerSdkBuildOutcome {owner:Arc<Mutex<BuildState>>,sdk:Arc<CompilerSdkClient>,input:BuildInputView,
    sdk_environment:ObjectRef,toolchains:BTreeMap<String,ToolchainIdentity>,action_key:ObjectRef,actions:Vec<ObjectRef>,checks:serde_json::Value}
pub(super) struct BuildState {registered:bool,sdk:Arc<CompilerSdkClient>,original:FinalizationBudget,cancelled:Arc<AtomicBool>,store:LocalStore,
    input:BuildInputView,profile:BuildProfile,nonce:String,caller_thread:std::thread::ThreadId,dispatch_permit:Option<NeverDispatched>,dispatch:Option<JobBinding>,execution:Option<OriginalSdkExecution>,terminal_failure:Option<terminal::OriginalSdkTerminalFailure>,disposal:Option<Arc<Mutex<settlement::DisposalState>>>,work:PathBuf,parent:Option<File>,work_fd:Option<File>,phase:&'static str,stage_attempted:bool,publish_attempted:bool,source:Option<OwnedFd>,output:Option<OwnedFd>,cancel:Option<OwnedFd>,
    reply:Option<RuntimeEndpoint>,remote_reply:Option<OwnedFd>,request:Option<Vec<u8>>,pending:Option<RuntimePacket>,sent:bool,
    stage:Option<StagingLease>,source_pin:String,result_pin:String,sdk_environment:Option<ObjectRef>,key:Option<ObjectRef>,
    artifacts:Option<ObjectRef>,result:Option<ObjectRef>,checks:Option<serde_json::Value>,published:bool,finished:bool,failed:bool,error:Option<String>,panicked:bool}
impl RetainedCompilerSdkBuild {pub fn outcome(&self)->Result<&CompilerSdkBuildOutcome>{self.outcome.as_ref().context("SDK build lacks its original successful publication")}
    pub fn failure_data(&self)->Result<Option<String>>{Ok(self.state.lock().map_err(|_|anyhow::anyhow!("SDK build custody poisoned"))?.error.clone())}}
impl CompilerSdkBuildOutcome {
    pub fn source_root(&self)->&ObjectRef{self.input.source_root()}
    pub fn input_topology(&self)->Option<&ObjectRef>{self.input.hardlinks()}
    pub fn checkpoint(&self)->&ObjectRef{self.input.checkpoint()}
    pub fn view_root(&self)->&ObjectRef{self.input.view_root()}
    pub fn sdk_environment(&self)->&ObjectRef{&self.sdk_environment}
    pub fn toolchains(&self)->&BTreeMap<String,ToolchainIdentity>{&self.toolchains}
    pub fn action_key(&self)->&ObjectRef{&self.action_key}
    pub fn action_results(&self)->&[ObjectRef]{&self.actions}
    pub fn artifacts(&self)->Result<ObjectRef>{let owner=self.owner.lock().map_err(|_|anyhow::anyhow!("SDK actual artifact owner poisoned"))?;owner.artifacts.clone().context("SDK actual artifact root absent")}
    pub fn checks(&self)->&serde_json::Value{&self.checks}
    /// Preparation reuse and compiler confinement do not qualify deterministic
    /// SDK execution. No ordinary outcome can claim an action-cache hit.
    pub fn warm_reuse(&self)->bool{false}
    pub fn qualified_owner(&self)->&Arc<CompilerSdkClient>{&self.sdk}
    pub fn current(&self,original:&FinalizationBudget)->Result<()>{original.remaining()?;self.sdk.current(original)?;
        let owner=self.owner.lock().map_err(|_|anyhow::anyhow!("SDK original result custody poisoned"))?;owner.result_current()?;original.remaining()?;Ok(())}
}
impl CompilerSdkClient {
    /// OFF App, blocking caller. The supplied original30 budget is never reset.
    /// Carrier and machine alias are installed before staging/export/send. On
    /// Err/unwind, owners, original FDs and uncertain side effects stay retained.
    /// A previous outcome is accepted for caller compatibility only. It never
    /// authorizes reuse, even for identical inputs or a reused preparation floor.
    pub fn build_selected_source_retaining(self:&Arc<Self>,out:&mut Option<RetainedCompilerSdkBuild>,input:&BuildInputView,
        profile:&BuildProfile,store:&LocalStore,owned_work_parent:&Path,original:FinalizationBudget,cancelled:Arc<AtomicBool>,
        _previous:Option<&CompilerSdkBuildOutcome>)->Result<()> {
        ensure!(out.is_none(),"SDK original caller output occupied");original.remaining()?;self.require_ordinary_execution()?;
        let nonce=uuid::Uuid::new_v4().to_string();let state=Arc::new(Mutex::new(BuildState{registered:false,sdk:self.clone(),original:original.clone(),cancelled:cancelled.clone(),store:store.clone(),
            input:input.clone(),profile:profile.clone(),nonce:nonce.clone(),caller_thread:std::thread::current().id(),dispatch_permit:Some(NeverDispatched),dispatch:None,execution:None,terminal_failure:None,disposal:None,work:owned_work_parent.join(format!("rudder-sdk-build-{nonce}")),parent:None,work_fd:None,phase:"staged",stage_attempted:false,publish_attempted:false,source:None,output:None,cancel:None,reply:None,remote_reply:None,
            request:None,pending:None,sent:false,stage:None,source_pin:format!("compiler-sdk-source/{nonce}"),result_pin:format!("compiler-sdk-result/{nonce}"),sdk_environment:None,key:None,artifacts:None,result:None,checks:None,
            published:false,finished:false,failed:false,error:None,panicked:false}));
        *out=Some(RetainedCompilerSdkBuild{state:state.clone(),outcome:None,failure:None});
        settlement::register_active_retaining(self,&state,&original,&cancelled)?;
        let attempt=catch_unwind(AssertUnwindSafe(||->Result<CompilerSdkBuildOutcome>{let mut held=state.lock().unwrap_or_else(|p|p.into_inner());held.prepare(owned_work_parent)?;
            // Mandatory proc-less confinement is not deterministic execution:
            // clock, randomness and interpreted code remain unqualified. Neither
            // preparation-cache evidence nor a matching key grants SDK reuse.
            held.export_and_capture()?;
            loop{if held.poll_root()?{break}std::thread::sleep(held.original.remaining()?.min(Duration::from_millis(1)));}
            held.publish_artifacts()?;held.outcome(state.clone())
        }));
        match attempt{Ok(Ok(result))=>{out.as_mut().unwrap().outcome=Some(result);Ok(())},Ok(Err(error))=>{
            let mut held=state.lock().unwrap_or_else(|p|p.into_inner());held.failed=true;out.as_mut().unwrap().failure=held.retain_failure_owner(state.clone());
            held.error=Some(format!("{}: {error:#}",held.phase));if out.as_ref().unwrap().failure.is_none(){if let Some(fd)=&held.cancel{let _=signal_cancel(fd);}}Err(error)
        },Err(payload)=>{let mut held=state.lock().unwrap_or_else(|p|p.into_inner());held.failed=true;held.panicked=true;held.error=Some("SDK original build panicked; retained".into());if let Some(fd)=&held.cancel{let _=signal_cancel(fd);}drop(held);std::panic::resume_unwind(payload)}}
    }
}
impl BuildState {
    fn live(&self)->Result<()>{self.original_paths_current()?;ensure!(!self.failed&&!self.cancelled.load(Ordering::Acquire),"SDK original build cancelled/failed");self.original.remaining()?;self.sdk.require_ordinary_execution()?;Ok(())}
    fn prepare(&mut self,parent:&Path)->Result<()>{self.phase="validate-profile-and-parent";self.live()?;validate_ordinary_profile(&self.profile)?;
        let m=fs::symlink_metadata(parent)?;ensure!(m.is_dir()&&!m.file_type().is_symlink()&&m.uid()==1500&&m.gid()==1500&&m.mode()&0o077==0&&parent.canonicalize()?==parent,"SDK work parent is not actual private Native directory");
        self.parent=Some(File::from(open_dir(parent)?));self.original_paths_current()?;
        ensure!(self.input.hardlink_group_count()==0,"A2 compiler adapter refuses unimplemented source hardlink topology");
        let sdk=self.sdk.preparation_spec_data()?;ensure!(self.profile.platform==sdk.platform,"SDK build platform differs from actual compiler closure");
        ensure!(self.profile.buck2.executable_sha256==sdk.toolchains["buck2"].configuration["executable_raw_sha256"],"profile Buck2 input differs from qualified SDK");
        self.phase="begin-original-cas-staging";self.stage_attempted=true;self.stage=Some(self.store.begin_staging(self.original.remaining()?)?);
        self.phase="publish-semantic-data";let environment=self.store.put_blob(&canonical_json(sdk)?)?;self.sdk_environment=Some(environment);
        let key=semantic_action_data(self.input.source_root(),self.input.hardlinks(),sdk,&self.profile)?;
        let mut references=vec![self.input.source_root().clone(),self.sdk_environment.as_ref().unwrap().clone()];if let Some(topology)=self.input.hardlinks(){references.push(topology.clone());}references.sort();references.dedup();
        self.key=Some(self.store.put_metadata(ObjectKind::ActionKey,&ReferencedMetadata{format_version:1,references,payload:serde_json::json!({"schema":"compiler-sdk-selected-action-v3","value":key})})?);self.live()?;
        // Pin the actual local selected source before private materialization.
        // The intended name lives in this owner before any commit attempt.
        Ok(())}
    fn export_and_capture(&mut self)->Result<()>{self.phase="pin-selected-source";self.live()?;self.store.pin(&self.source_pin,self.input.source_root())?;self.live()?;fs::DirBuilder::new().mode(0o700).create(&self.work)?;self.work_fd=Some(File::from(open_dir(&self.work)?));self.phase="export-and-verify-selected-source";
        let raw=self.store.read_object(self.input.source_root())?;let source=rudder_engine::snapshot::decode_source(&raw)?;
        let hardlinks=if let Some(actual)=self.input.hardlinks(){actual.clone()}else{self.store.put_blob(&canonical_json(&rudder_engine::tree::HardlinkTopology{format_version:1,groups:vec![]})?)?};
        rudder_engine::tree::export_quiesced(&self.store,&rudder_engine::tree::TreeSnapshot{format_version:1,directory:source.directory.clone(),hardlinks},&self.work.join("source"),&source_budget())?;
        self.source=Some(open_dir(&self.work.join("source"))?);
        verify_source(&self.store,&source.directory,self.source.as_ref().unwrap(),&self.profile,&self.original)?;
        fs::DirBuilder::new().mode(0o700).create(self.work.join("output"))?;
        self.output=Some(open_dir(&self.work.join("output"))?);self.cancel=Some(cancel_fd()?);
        let(local,remote)=wire::runtime_socket_pair()?;self.reply=Some(RuntimeEndpoint::capture(local)?);self.remote_reply=Some(remote);
        let registration=self.sdk.client.registration.data();let request=super::super::compiler_sdk::jobs::BuildRequest{version:2,kind:"compiler_sdk_build_v2".into(),serial:0,nonce:self.nonce.clone(),
            engine_instance_id:registration.engine_instance_id.clone(),epoch:registration.binding.epoch.clone(),config_sha256:registration.binding.config_sha256.clone(),constraint:self.original.constraint(),
            source_root:self.input.source_root().clone(),input_topology:self.input.hardlinks().cloned(),action_key:self.key.as_ref().unwrap().clone(),profile:self.profile.clone()};
        self.request=Some(canonical_json(&request)?);ensure!(self.request.as_ref().unwrap().len()<=16*1024,"SDK selected profile exceeds original packet bound");self.live()?;Ok(())}
    fn poll_root(&mut self)->Result<bool>{self.phase="actual-root-compiler-child-and-pipe-settlement";self.live()?;
        if !self.sent{let client=match self.sdk.client.state.try_lock(){Ok(v)=>v,Err(TryLockError::WouldBlock)=>return Ok(false),Err(TryLockError::Poisoned(_))=>anyhow::bail!("SDK original runtime transport poisoned")};
            if client.active.is_some()||client.backing_active.is_some(){return Ok(false)}
            self.dispatch_permit.take().context("SDK original dispatch already attempted; no replay")?;
            let serial=self.sdk.dispatch_serial.fetch_update(Ordering::AcqRel,Ordering::Acquire,|n|n.checked_add(1)).map_err(|_|anyhow::anyhow!("SDK dispatch serial exhausted"))?+1;
            let mut request:super::super::compiler_sdk::jobs::BuildRequest=serde_json::from_slice(self.request.as_ref().unwrap())?;
            request.serial=serial;request.nonce=job_protocol::dispatch_nonce(&request.engine_instance_id,&request.epoch,&request.config_sha256,serial)?;
            self.dispatch=Some(JobBinding{serial,nonce:request.nonce.clone(),engine_instance_id:request.engine_instance_id.clone(),epoch:request.epoch.clone(),config_sha256:request.config_sha256.clone(),source_root:request.source_root.clone(),action_key:request.action_key.clone(),build_constraint:request.constraint.clone()});
            self.request=Some(canonical_json(&request)?);
            match client.endpoint.send_data(self.request.as_ref().unwrap(),&[self.source.as_ref().unwrap(),self.output.as_ref().unwrap(),self.cancel.as_ref().unwrap(),self.remote_reply.as_ref().unwrap()])?{
                false=>{self.dispatch_permit=Some(NeverDispatched);self.dispatch=None;return Ok(false)},true=>self.sent=true};}
        if self.pending.is_none(){match self.reply.as_ref().unwrap().receive_disposal_retaining(&mut self.pending)?{
            wire::RuntimeDisposalRead::Pending=>return Ok(false),wire::RuntimeDisposalRead::Packet=>{},wire::RuntimeDisposalRead::ReadClosed{..}=>anyhow::bail!("SDK original build endpoint closed; original resources retained")}}
        let Some(packet)=&self.pending else{return Ok(false)};
        ensure!(packet.credentials.pid==self.sdk.client.service.pid as i32&&packet.credentials.uid==0&&packet.credentials.gid==0&&packet.fds.is_empty(),"SDK ACK is not actual original Root service");
        let value:serde_json::Value=serde_json::from_slice(&packet.bytes)?;
        if value["kind"]=="compiler_sdk_helper_terminal_failure_v1"{return self.poll_terminal_ack_retaining()}
        let mount=self.sdk.mount.as_ref().unwrap();
        ensure!(value["version"]==2&&value["kind"]=="compiler_sdk_build_ack_v2"&&value["nonce"]==self.dispatch.as_ref().unwrap().nonce&&value["serial"]==self.dispatch.as_ref().unwrap().serial&&value["action_key"]==serde_json::to_value(self.key.as_ref().unwrap())?
            &&value["source_root"]==serde_json::to_value(self.input.source_root())?&&value["sdk"]==serde_json::to_value(mount)?&&value["child_exit_code"]==0&&value["stdout_eof"]==true&&value["stderr_eof"]==true,"SDK actual build ACK differs");
        let report:super::super::compiler_sdk::CompilerSdkProbeData=serde_json::from_value(value["result"].clone())?;
        ensure!(report.original_constraint==self.original.constraint()&&report.workload["kind"]=="actual-selected-sdk-build-v1"&&report.workload["action_key"]==value["action_key"]&&report.workload["source_root"]==value["source_root"]
            &&report.workload["buck2_frontend_executed"]==false,"SDK execution result differs");
        let checks=report.workload["checks"].as_array().context("actual SDK compiler checks absent")?;ensure!(checks.len()==self.profile.targets.len(),"SDK target count differs");
        for(actual,expected)in checks.iter().zip(&self.profile.targets){ensure!(actual["target"]==expected.name&&actual["language"]==serde_json::to_value(expected.language)?&&actual["exit_code"]==0,"SDK actual target failed/differs");}
        super::super::compiler_sdk::validate_selected_rules_workload(&report.workload,&serde_json::json!({"profile":self.profile}))?;
        self.execution=Some(OriginalSdkExecution{binding:self.dispatch.as_ref().unwrap().clone(),helper_pid:report.helper_pid,helper_start:report.helper_start_ticks.clone(),ready:false});
        self.checks=Some(report.workload);self.live()?;self.execution.as_mut().unwrap().ready=true;Ok(true)}
    fn publish_artifacts(&mut self)->Result<()>{self.phase="revalidate-source-and-import-artifacts";self.live()?;
        let directory=rudder_engine::snapshot::decode_source(&self.store.read_object(self.input.source_root())?)?.directory;
        // Actual input revalidation catches a changed private source, without
        // deriving success from directory permissions or original report DATA.
        verify_source(&self.store,&directory,self.source.as_ref().unwrap(),&self.profile,&self.original)?;
        validate_output_tree(&self.work.join("output"),&self.original)?;self.live()?;
        let tree=rudder_engine::tree::import_quiesced(&self.store,&self.stage.as_ref().unwrap().id,&self.work.join("output"),&Default::default(),&rudder_engine::tree::ImportBudget{max_logical_bytes:MAX_OUTPUT_BYTES,..source_budget()})?;
        ensure!(serde_json::from_slice::<rudder_engine::tree::HardlinkTopology>(&self.store.read_object(&tree.hardlinks)?)?.groups.is_empty(),"SDK outputs contain unmodeled hardlinks");self.artifacts=Some(tree.directory);self.live()?;
        self.result=Some(self.store.put_metadata(ObjectKind::ActionResult,&ordinary_artifact_receipt(
            self.key.as_ref().unwrap(),self.artifacts.as_ref().unwrap(),self.checks.as_ref().context("SDK actual checks absent")?,&self.nonce))?);self.live()?;
        self.phase="publish-actual-artifact-result";self.publish_attempted=true;self.store.publish(&self.stage.as_ref().unwrap().id,&self.result_pin,self.result.as_ref().unwrap())?;self.published=true;self.live()?;self.finished=true;self.phase="completed";Ok(())}
    fn original_paths_current(&self)->Result<()>{
        fn same(path:&Path,fd:&File)->Result<()>{let a=fd.metadata()?;let n=fs::symlink_metadata(path)?;ensure!(n.is_dir()&&!n.file_type().is_symlink()&&a.is_dir()&&a.uid()==1500&&a.gid()==1500
            &&(a.dev(),a.ino(),a.uid(),a.gid(),a.mode())==(n.dev(),n.ino(),n.uid(),n.gid(),n.mode()),"SDK original private directory moved/replaced");Ok(())}
        if let Some(parent)=&self.parent{same(self.work.parent().context("SDK work parent absent")?,parent)?;}if let Some(work)=&self.work_fd{same(&self.work,work)?;}
        if let Some(source)=&self.source{same(&self.work.join("source"),&File::from(source.try_clone()?))?;}if let Some(output)=&self.output{same(&self.work.join("output"),&File::from(output.try_clone()?))?;}Ok(())
    }
    fn result_current(&self)->Result<()>{ensure!(self.finished&&!self.failed,"SDK original result not successful");self.sdk.assert_current()?;
        ensure!(self.published&&self.store.retention()?.pins.get(&self.result_pin)==self.result.as_ref(),"SDK result original pin absent");
        self.store.retained_closure(self.result.as_ref().unwrap())?;Ok(())}
    fn outcome(&self,owner:Arc<Mutex<BuildState>>)->Result<CompilerSdkBuildOutcome>{self.live()?;ensure!(self.finished,"SDK execution/publication incomplete");
        Ok(CompilerSdkBuildOutcome{owner,sdk:self.sdk.clone(),input:self.input.clone(),sdk_environment:self.sdk_environment.as_ref().unwrap().clone(),toolchains:self.sdk.toolchains_data()?.clone(),
            action_key:self.key.as_ref().unwrap().clone(),actions:vec![self.result.as_ref().unwrap().clone()],checks:self.checks.clone().context("SDK actual checks absent")?})}
    pub(super) fn signal_original_cancellation(&self)->Result<()>{if self.cancelled.load(Ordering::Acquire)||self.original.remaining().is_err(){if let Some(fd)=&self.cancel{signal_cancel(fd)?;}}Ok(())}
}
// This validates admission to the fixed compiler runner, not cacheability of
// its result. An empty declared effects set cannot prove deterministic code.
fn validate_ordinary_profile(profile:&BuildProfile)->Result<()> {
    profile.validate()?;
    ensure!(profile.policy.cacheable()&&profile.buck2.remote_cache.is_none()
        &&profile.policy.environment.keys().all(|k|matches!(k.as_str(),"LANG"|"LC_ALL"|"TZ")),
        "direct SDK compiler refuses unsupported effects/environment/remote cache");
    for target in &profile.targets {
        ensure!(target.name.len()<=128,"SDK target name exceeds runner bound");
        for path in &target.sources {
            ensure!(path.len()<=1024&&path.split('/').all(|part|!matches!(part,""|"."|"..")),
                "SDK source path is not a bounded normalized runner path");
        }
        // Both rustc and tsc expand @response files and parse leading options.
        // The fixed runner inserts the entrypoint directly into argv. Its
        // second placeholder substitution must not rewrite the input either.
        ensure!(!target.entrypoint.starts_with(['-','@'])
            &&!target.entrypoint.contains("{output}")&&!target.entrypoint.contains("{entrypoint}"),
            "SDK entrypoint could change the fixed compiler invocation");
    }
    Ok(())
}
// A retained artifact receipt is deliberately not action-cache provenance.
// No preparation acknowledgment, confinement observation or previous result
// can switch this policy. Enabling reuse needs real supported-profile proof.
fn ordinary_artifact_receipt(key:&ObjectRef,artifacts:&ObjectRef,checks:&serde_json::Value,nonce:&str)->ReferencedMetadata {
    ReferencedMetadata{format_version:1,references:vec![key.clone(),artifacts.clone()],
        payload:serde_json::json!({"schema":"compiler-sdk-selected-result-v1","action_key":key,"artifacts":artifacts,"checks":checks,"buck2_frontend_executed":false,
            "execution_nonce":nonce,"cacheable":false,"cache_reuse":"disabled-until-hermetic-inputs-enforced"})}
}
fn semantic_action_data(source:&ObjectRef,topology:Option<&ObjectRef>,sdk:&PreparationSpec,profile:&BuildProfile)->Result<serde_json::Value>{
    semantic_action_with_rules_data(source,topology,sdk,profile,&sdk_execution_rules_data()?)
}
fn semantic_action_with_rules_data(source:&ObjectRef,topology:Option<&ObjectRef>,sdk:&PreparationSpec,profile:&BuildProfile,rules:&serde_json::Value)->Result<serde_json::Value>{
    semantic_action_with_policies_data(source,topology,sdk,profile,rules,
        &serde_json::from_str(include_str!("sdk_compiler_isolation/policy.json"))?)
}
fn semantic_action_with_policies_data(source:&ObjectRef,topology:Option<&ObjectRef>,sdk:&PreparationSpec,profile:&BuildProfile,rules:&serde_json::Value,isolation:&serde_json::Value)->Result<serde_json::Value>{
    // Key the mechanism's semantic policy, not a hash of observer source code
    // or an observation supplied by a previous execution.
    Ok(serde_json::json!({"format_version":3,"kind":"compiler-sdk-selected-action-v3","source_root":source,"input_topology":topology,"sdk":sdk,
        "platform":profile.platform,"policy":profile.policy,"targets":profile.targets,"execution_rules":rules,"compiler_isolation":isolation}))
}
fn sdk_execution_rules_data()->Result<serde_json::Value>{Ok(serde_json::from_str(include_str!("compiler_sdk_rules.json"))?)}
fn open_dir(path:&Path)->Result<OwnedFd>{Ok(OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY|libc::O_NOFOLLOW|libc::O_CLOEXEC).open(path)?.into())}
fn source_budget()->rudder_engine::tree::ImportBudget{rudder_engine::tree::ImportBudget{max_entries:MAX_SOURCE_FILES as u64,max_logical_bytes:MAX_SOURCE_BYTES,max_depth:256,max_metadata_bytes:128*1024*1024}}
fn validate_output_tree(path:&Path,original:&FinalizationBudget)->Result<()>{fn walk(path:&Path,original:&FinalizationBudget,count:&mut usize,bytes:&mut u64,depth:usize)->Result<()>{
    original.remaining()?;ensure!(depth<=256,"SDK output depth exceeds bound");let m=fs::symlink_metadata(path)?;*count+=1;ensure!(*count<=MAX_SOURCE_FILES,"SDK output entries exceed bound");
    if m.is_dir()&&!m.file_type().is_symlink(){for entry in fs::read_dir(path)?{walk(&entry?.path(),original,count,bytes,depth+1)?;}}
    else{ensure!(m.is_file()&&!m.file_type().is_symlink()&&m.nlink()==1,"SDK output symlink/special/hardlink refused");*bytes=bytes.checked_add(m.len()).context("SDK output bytes overflow")?;ensure!(*bytes<=MAX_OUTPUT_BYTES,"SDK output byte bound exceeded");}original.remaining()?;Ok(())}
    walk(path,original,&mut 0,&mut 0,0)}
fn verify_source(store:&LocalStore,root:&ObjectRef,source:&OwnedFd,profile:&BuildProfile,original:&FinalizationBudget)->Result<()>{
    validate_ordinary_profile(profile)?;
    let mut declared:std::collections::BTreeSet<String>=profile.targets.iter().flat_map(|target|target.sources.iter().cloned()).collect();
    ensure!(declared.len()<=MAX_SOURCE_FILES,"SDK declared source count exceeds bound");
    // Anchor every lookup to the same source descriptor sent to Root. A final
    // O_NOFOLLOW alone does not stop a replaced ancestor from redirecting reads.
    fn child(parent:&File,name:&std::ffi::OsStr,directory:bool)->Result<File>{
        let name=std::ffi::CString::new(name.as_encoded_bytes())?;
        let raw=unsafe{libc::openat(parent.as_raw_fd(),name.as_ptr(),libc::O_RDONLY|libc::O_NOFOLLOW|libc::O_CLOEXEC|libc::O_NONBLOCK|if directory{libc::O_DIRECTORY}else{0})};
        ensure!(raw>=0,"SDK source descriptor open failed: {}",io::Error::last_os_error());
        Ok(unsafe{File::from_raw_fd(raw)})
    }
    fn unchanged(before:&fs::Metadata,after:&fs::Metadata)->Result<()>{
        ensure!((before.dev(),before.ino(),before.mode(),before.uid(),before.gid(),before.nlink(),before.len(),before.mtime(),before.mtime_nsec(),before.ctime(),before.ctime_nsec())
            ==(after.dev(),after.ino(),after.mode(),after.uid(),after.gid(),after.nlink(),after.len(),after.mtime(),after.mtime_nsec(),after.ctime(),after.ctime_nsec()),
            "SDK selected source changed during verification");Ok(())
    }
    fn walk(store:&LocalStore,root:&ObjectRef,directory:&File,prefix:&str,declared:&mut std::collections::BTreeSet<String>,original:&FinalizationBudget,depth:usize,count:&mut u64,bytes:&mut u64)->Result<()>{
        original.remaining()?;ensure!(depth<=256,"SDK source depth exceeds bound");let manifest=rudder_engine::tree::root_info(store,root)?;
        *count=count.checked_add(manifest.entry_count).context("SDK source entries overflow")?;ensure!(*count<=MAX_SOURCE_FILES as u64,"SDK source count exceeds bound");
        let m=directory.metadata()?;ensure!(m.is_dir()&&m.mode()&0o7777==manifest.mode,"SDK source directory type/mode differs");
        let mut actual=std::collections::BTreeSet::new();for entry in fs::read_dir(format!("/proc/self/fd/{}",directory.as_raw_fd()))?{original.remaining()?;ensure!(actual.len()<MAX_SOURCE_FILES,"SDK source directory count exceeds bound");actual.insert(entry?.file_name());}
        let mut expected=std::collections::BTreeSet::new();let mut after=None;
        loop{original.remaining()?;let page=rudder_engine::tree::scan(store,root,after.as_deref(),1024)?;
            for entry in page.entries{let name=std::ffi::OsString::from_vec(entry.name()?);expected.insert(name.clone());let path=format!("{prefix}{}",name.to_str().context("SDK source name is not UTF-8")?);
                match entry.node{rudder_engine::directory::EntryNode::Directory{object}=>walk(store,&object,&child(directory,&name,true)?,&format!("{path}/"),declared,original,depth+1,count,bytes)?,
                    rudder_engine::directory::EntryNode::File{object,logical_size,mode}=>{*bytes=bytes.checked_add(logical_size).context("SDK source byte overflow")?;ensure!(*bytes<=MAX_SOURCE_BYTES,"SDK source byte bound exceeded");
                        ensure!(declared.remove(&path),"SDK source file is outside the declared input closure: {path}");
                        let mut actual=child(directory,&name,false)?;let m=actual.metadata()?;
                        ensure!(m.is_file()&&m.nlink()==1&&m.len()==logical_size&&m.mode()&0o7777==mode,"SDK source file/type/mode differs");
                        let mut compare=SourceComparison{actual:&mut actual,original};ensure!(rudder_engine::content::export_file(store,&object,&mut compare)?.logical_size==logical_size,"SDK source logical length differs");
                        ensure!(actual.read(&mut[0u8;1])?==0,"SDK source trailing bytes differ");
                        unchanged(&m,&actual.metadata()?)?;
                        unchanged(&m,&child(directory,&name,false)?.metadata()?)?;
                    },rudder_engine::directory::EntryNode::Symlink{..}=>anyhow::bail!("managed SDK compiler refuses symlink source inputs"),}
            }after=page.next_after;if after.is_none(){break}
        }ensure!(actual==expected,"SDK source namespace differs");unchanged(&m,&directory.metadata()?)?;original.remaining()?;Ok(())
    }
    walk(store,root,&File::from(source.try_clone()?),"",&mut declared,original,0,&mut 0,&mut 0)?;
    ensure!(declared.is_empty(),"SDK declared source files are absent from the selected input closure");Ok(())
}
struct SourceComparison<'a>{actual:&'a mut File,original:&'a FinalizationBudget}
impl Write for SourceComparison<'_>{fn write(&mut self,expected:&[u8])->io::Result<usize>{let mut buffer=[0u8;65536];for chunk in expected.chunks(buffer.len()){
    self.original.remaining().map_err(io::Error::other)?;self.actual.read_exact(&mut buffer[..chunk.len()])?;if buffer[..chunk.len()]!=*chunk{return Err(io::Error::other("SDK selected source bytes changed"));}}
    Ok(expected.len())}fn flush(&mut self)->io::Result<()>{Ok(())}}

#[cfg(test)]mod tests{use super::*;
    fn data()->(ObjectRef,PreparationSpec,BuildProfile){
        use rudder_engine::build::{PlatformIdentity,Buck2Profile,BuildPolicy,CompileTarget,CompileLanguage};
        let source=ObjectRef::from_bytes(ObjectKind::SourceRoot,b"data-source");let directory=ObjectRef::from_bytes(ObjectKind::Directory,b"data-sdk");let exe=ObjectRef::from_bytes(ObjectKind::Blob,b"data-compiler");
        let platform=PlatformIdentity{os:"linux".into(),architecture:"x86_64".into(),abi:"gnu-glibc-2.36".into(),properties:BTreeMap::new()};
        let spec=PreparationSpec{format_version:1,platform:platform.clone(),base_runtime:directory.clone(),installer_inputs:BTreeMap::new(),installer_commands:vec![],installer_environment:BTreeMap::new(),
            toolchains:BTreeMap::from([("rustc".into(),ToolchainIdentity{version:"test-data".into(),closure:directory,executable:exe,executable_path:"tools/rustc".into(),configuration:BTreeMap::new()})]),policy_version:"test".into(),effects:Default::default()};
        let profile=BuildProfile{format_version:1,name:"label-a".into(),platform,buck2:Buck2Profile{release:"data".into(),executable_sha256:"a".repeat(64),remote_cache:None},
            policy:BuildPolicy{version:"hermetic-data".into(),environment:BTreeMap::new(),effects:Default::default()},targets:vec![CompileTarget{name:"library".into(),language:CompileLanguage::Rust,entrypoint:"lib.rs".into(),sources:std::collections::BTreeSet::from(["lib.rs".into()])}]};(source,spec,profile)
    }
    #[test]fn actual_semantic_key_changes_with_source_sdk_rules_and_targets_but_ignores_profile_label(){
        let(source,sdk,mut profile)=data();let base=semantic_action_data(&source,None,&sdk,&profile).unwrap();profile.name="label-b".into();
        assert_eq!(base,semantic_action_data(&source,None,&sdk,&profile).unwrap());
        assert_ne!(base,semantic_action_data(&ObjectRef::from_bytes(ObjectKind::SourceRoot,b"different"),None,&sdk,&profile).unwrap());
        let mut changed=sdk.clone();changed.toolchains.get_mut("rustc").unwrap().version="new-sdk".into();assert_ne!(base,semantic_action_data(&source,None,&changed,&profile).unwrap());
        profile.targets[0].entrypoint="changed.rs".into();profile.targets[0].sources.insert("changed.rs".into());assert_ne!(base,semantic_action_data(&source,None,&sdk,&profile).unwrap());
        let keys=base.as_object().unwrap();for key in ["checkpoint","view_root","runtime_environment","engine_instance_id","config_sha256","network"]{assert!(!keys.contains_key(key));}
    }
    #[test]fn actual_compiler_rule_data_and_platform_changes_change_the_key_without_control_source_hashes(){
        let(source,sdk,profile)=data();let rules=sdk_execution_rules_data().unwrap();let actual=semantic_action_data(&source,None,&sdk,&profile).unwrap();
        assert_eq!(actual,semantic_action_with_rules_data(&source,None,&sdk,&profile,&rules).unwrap());
        for field in ["python_home","source_root","sandbox_profile"]{let mut changed=rules.clone();changed[field]="different-semantic-rule".into();assert_ne!(actual,semantic_action_with_rules_data(&source,None,&sdk,&profile,&changed).unwrap());}
        let mut changed=profile.clone();changed.platform.abi="changed-ABI".into();assert_ne!(actual,semantic_action_data(&source,None,&sdk,&changed).unwrap());
        let mut changed=sdk.clone();changed.toolchains.get_mut("rustc").unwrap().configuration.insert("compiler_entrypoint".into(),"new/path".into());assert_ne!(actual,semantic_action_data(&source,None,&changed,&profile).unwrap());
        let text=canonical_json(&actual).unwrap();let text=String::from_utf8(text).unwrap();for field in ["isolation_sha256","rules_sha256","artifact_manifest_sha256","source_binding_sha256","cas_image","qualification_workload"]{assert!(!text.contains(field));}
    }
    #[test]fn actual_isolation_policy_is_keyed_independently_of_observer_reports(){
        let(source,sdk,profile)=data();let rules=sdk_execution_rules_data().unwrap();
        let isolation:serde_json::Value=serde_json::from_str(include_str!("sdk_compiler_isolation/policy.json")).unwrap();
        let base=semantic_action_data(&source,None,&sdk,&profile).unwrap();
        assert_eq!(base["format_version"],3);assert_eq!(base["compiler_isolation"],isolation);
        for (field,value) in [("source_declarations",serde_json::json!("unchecked")),
            ("native_writable_exec",serde_json::json!("allowed")),("syscall_additions",serde_json::json!({})),
            ("mounts",serde_json::json!([])),("cache_reuse",serde_json::json!(true))] {
            let mut changed=isolation.clone();changed[field]=value;
            assert_ne!(base,semantic_action_with_policies_data(&source,None,&sdk,&profile,&rules,&changed).unwrap(),"{field}");
        }
        let topology=ObjectRef::from_bytes(ObjectKind::Blob,b"topology");
        assert_ne!(base,semantic_action_data(&source,Some(&topology),&sdk,&profile).unwrap());
    }
    #[test]fn ordinary_sdk_admission_refuses_every_declared_effect_and_ambient_input(){
        use rudder_engine::build::{BuildEffect,RemoteCacheConfig};
        let(_,_,profile)=data();validate_ordinary_profile(&profile).unwrap();
        for effect in [BuildEffect::Network,BuildEffect::Secrets,BuildEffect::Signing,BuildEffect::Deployment,
            BuildEffect::ExternalState,BuildEffect::Clock,BuildEffect::Randomness] {
            let mut changed=profile.clone();changed.policy.effects.insert(effect);
            assert!(validate_ordinary_profile(&changed).is_err(),"{effect:?}");
        }
        for name in ["PATH","HOME","SOURCE_DATE_EPOCH","SDK_SECRET","RANDOM"] {
            let mut changed=profile.clone();changed.policy.environment.insert(name.into(),"unmodeled".into());
            assert!(validate_ordinary_profile(&changed).is_err(),"{name}");
        }
        let mut changed=profile.clone();changed.buck2.remote_cache=Some(RemoteCacheConfig{cas_address:"http://localhost:1234".into(),
            action_cache_address:"http://localhost:1234".into(),instance_name:"sdk".into(),tls:false});
        assert!(validate_ordinary_profile(&changed).is_err());
    }
    #[test]fn ordinary_sdk_profile_cannot_supply_shell_commands_or_override_fixed_compilers(){
        let(_,_,profile)=data();let original=serde_json::to_value(profile).unwrap();
        for field in ["command","shell","rust_argv","installer_commands"] {
            let mut changed=original.clone();changed[field]=serde_json::json!(["/bin/sh","-c","date; cat /run/secrets/token"]);
            assert!(serde_json::from_value::<BuildProfile>(changed).is_err(),"{field}");
            let mut changed=original.clone();changed["targets"][0][field]=serde_json::json!(["/bin/sh","-c","date"]);
            assert!(serde_json::from_value::<BuildProfile>(changed).is_err(),"target {field}");
        }
        let mut changed=original;changed["targets"][0]["language"]="shell".into();
        assert!(serde_json::from_value::<BuildProfile>(changed).is_err());
    }
    #[test]fn ordinary_sdk_entrypoint_cannot_inject_options_response_files_or_placeholders(){
        let(_,_,profile)=data();
        for language in [rudder_engine::build::CompileLanguage::Rust,rudder_engine::build::CompileLanguage::TypeScript] {
            for entrypoint in ["--version","--cfg=unmodeled","@compiler-options","{output}","src/{output}.rs","{entrypoint}","src//lib.rs","src/./lib.rs","lib.rs/"] {
                let mut changed=profile.clone();changed.targets[0].language=language;
                changed.targets[0].entrypoint=entrypoint.into();changed.targets[0].sources=std::collections::BTreeSet::from([entrypoint.into()]);
                assert!(validate_ordinary_profile(&changed).is_err(),"{language:?}: {entrypoint}");
            }
        }
        let mut changed=profile.clone();changed.targets[0].name="x".repeat(129);assert!(validate_ordinary_profile(&changed).is_err());
        let mut changed=profile.clone();changed.targets[0].sources.insert(format!("{}.rs","x".repeat(1024)));assert!(validate_ordinary_profile(&changed).is_err());
        let mut changed=profile;changed.targets[0].entrypoint="src/@ordinary.rs".into();changed.targets[0].sources=std::collections::BTreeSet::from(["src/@ordinary.rs".into()]);
        validate_ordinary_profile(&changed).unwrap();
    }
    #[test]fn ordinary_sdk_identical_semantics_and_preparation_evidence_never_publish_cache_provenance(){
        let(source,sdk,profile)=data();let semantic=semantic_action_data(&source,None,&sdk,&profile).unwrap();
        let key=ObjectRef::from_bytes(ObjectKind::ActionKey,&canonical_json(&semantic).unwrap());
        let artifact=ObjectRef::from_bytes(ObjectKind::Directory,b"artifact-data-only");
        // DATA-only adversarial reports cannot turn artifact publication into
        // compiler qualification, even when they claim successful confinement.
        let checks=serde_json::json!({"preparation_cache_hit":true,"sealed_inputs":true,
            "confinement_enforced":true,"qualification":"passed","cacheable":true,"cache_reuse":true});
        let first=ordinary_artifact_receipt(&key,&artifact,&checks,"original-1");
        let second=ordinary_artifact_receipt(&key,&artifact,&checks,"original-2");
        for receipt in [&first,&second] {
            assert_eq!(receipt.references,vec![key.clone(),artifact.clone()]);
            assert_eq!(receipt.payload["action_key"],serde_json::to_value(&key).unwrap());
            assert_eq!(receipt.payload["cacheable"],false);
            assert_eq!(receipt.payload["cache_reuse"],"disabled-until-hermetic-inputs-enforced");
            assert_eq!(receipt.payload["buck2_frontend_executed"],false);
        }
        assert_ne!(canonical_json(&first).unwrap(),canonical_json(&second).unwrap());
    }
    struct Scratch(PathBuf);
    impl Scratch {fn new()->Self{let path=PathBuf::from("/workspace/rudder-scratch/harden-9").join(uuid::Uuid::new_v4().to_string());fs::create_dir_all(&path).unwrap();Self(path)}}
    impl Drop for Scratch {fn drop(&mut self){fs::remove_dir_all(&self.0).unwrap();}}
    struct SourceFixture {scratch:Scratch,store:LocalStore,tree:rudder_engine::tree::TreeSnapshot,source:OwnedFd,profile:BuildProfile,original:FinalizationBudget}
    impl SourceFixture {
        fn new(files:&[(&str,&[u8])])->Self {
            let scratch=Scratch::new();let store=LocalStore::open(scratch.0.join("cas")).unwrap();let input=scratch.0.join("input");fs::create_dir(&input).unwrap();
            for (name,bytes) in files {let path=input.join(name);fs::create_dir_all(path.parent().unwrap()).unwrap();fs::write(path,bytes).unwrap();}
            let stage=store.begin_staging(Duration::from_secs(30)).unwrap();let tree=rudder_engine::tree::import_quiesced(&store,&stage.id,&input,&Default::default(),&source_budget()).unwrap();store.publish(&stage.id,"sdk-source-test",&tree.directory).unwrap();
            let exported=scratch.0.join("exported");rudder_engine::tree::export_quiesced(&store,&tree,&exported,&source_budget()).unwrap();let source=open_dir(&exported).unwrap();
            let(_,_,mut profile)=data();profile.targets[0].entrypoint=files[0].0.into();profile.targets[0].sources=files.iter().map(|(name,_)|(*name).into()).collect();
            Self{scratch,store,tree,source,profile,original:FinalizationBudget::begin(Duration::from_secs(30)).unwrap()}
        }
        fn path(&self)->PathBuf{self.scratch.0.join("exported")}
        fn verify(&self)->Result<()>{verify_source(&self.store,&self.tree.directory,&self.source,&self.profile,&self.original)}
    }
    #[test]fn actual_paged_chunked_source_export_verifies_then_detects_changed_tail(){
        let bytes=vec![b'x';rudder_engine::content::CHUNK_BYTES+31];let fixture=SourceFixture::new(&[("lib.rs",&bytes)]);
        fixture.verify().unwrap();OpenOptions::new().write(true).open(fixture.path().join("lib.rs")).unwrap().write_at(b"z",rudder_engine::content::CHUNK_BYTES as u64+30).unwrap();
        assert!(fixture.verify().is_err());
    }
    #[test]fn actual_selected_source_requires_exact_declared_job_union(){
        let mut fixture=SourceFixture::new(&[("lib.rs",b"mod dependency;"),("dependency.rs",b"pub fn value() {}")]);
        fixture.profile.targets[0].sources.remove("dependency.rs");assert!(fixture.verify().is_err());
        let mut second=fixture.profile.targets[0].clone();second.name="second".into();second.entrypoint="dependency.rs".into();second.sources=std::collections::BTreeSet::from(["dependency.rs".into()]);fixture.profile.targets.push(second);
        fixture.verify().unwrap();fixture.profile.targets[0].sources.insert("missing.rs".into());assert!(fixture.verify().is_err());
    }
    #[test]fn actual_selected_source_refuses_extra_files_and_ancestor_symlinks(){
        let fixture=SourceFixture::new(&[("src/lib.rs",b"pub fn value() {}")]);fixture.verify().unwrap();
        fs::write(fixture.path().join("undeclared"),b"ambient").unwrap();assert!(fixture.verify().is_err());fs::remove_file(fixture.path().join("undeclared")).unwrap();
        fs::rename(fixture.path().join("src"),fixture.scratch.0.join("elsewhere")).unwrap();
        std::os::unix::fs::symlink(fixture.scratch.0.join("elsewhere"),fixture.path().join("src")).unwrap();assert!(fixture.verify().is_err());
    }
    #[test]fn actual_selected_source_verifies_the_held_root_and_rejects_hardlinks_and_fifos(){
        let fixture=SourceFixture::new(&[("lib.rs",b"pub fn value() {}")]);fixture.verify().unwrap();
        fs::hard_link(fixture.path().join("lib.rs"),fixture.scratch.0.join("alias")).unwrap();assert!(fixture.verify().is_err());fs::remove_file(fixture.scratch.0.join("alias")).unwrap();
        fs::rename(fixture.path(),fixture.scratch.0.join("original")).unwrap();fs::create_dir(fixture.path()).unwrap();fs::write(fixture.path().join("lib.rs"),b"different").unwrap();
        fixture.verify().unwrap(); // Root receives this descriptor, not the new pathname.
        let path=fixture.scratch.0.join("original/lib.rs");fs::remove_file(&path).unwrap();
        let path=std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();assert_eq!(unsafe{libc::mkfifo(path.as_ptr(),0o600)},0);
        assert!(fixture.verify().is_err()); // Nonblocking open must refuse, never hang.
    }
    #[test]fn actual_output_validation_refuses_symlink_without_root_or_compiler_authority(){
        let scratch=Scratch::new();std::os::unix::fs::symlink("/etc/passwd",scratch.0.join("artifact")).unwrap();
        let original=FinalizationBudget::begin(Duration::from_secs(30)).unwrap();assert!(validate_output_tree(&scratch.0,&original).is_err());
    }
}
