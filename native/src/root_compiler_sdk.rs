//! Separate immutable SDK floor and original fixed qualification helper.
//! Private child of the genuine runtime mount issuer; UID/path/config DATA
//! alone cannot construct this owner. No Drop cleanup or CAS payload import.
use super::*;
use rudder_engine::{execution::compiler_sdk::{CompilerSdkCatalogData,CompilerSdkMountData,SDK_SOURCE},
    build::ToolchainIdentity,store::backed_inputs::{VerifiedBakedInputs,VerifiedBakedRead,CompilerSdkSemanticInputs}};
use std::{process::{Child,Command,ExitStatus,Stdio},time::Instant};
const SDK_ROOT:&str="/run/rudder-root-storage/compiler-sdk-v1";
const SDK_FLOOR:&str="/run/rudder-root-storage/compiler-sdk-v1/floor";
#[path="compiler_sdk_catalog_derivation.rs"]mod catalog_derivation;
pub const HELPER_ARG:&str="--compiler-sdk-qualification-helper";
#[derive(Clone,Debug,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompilerSdkProbeData {pub format_version:u32,pub kind:String,pub helper_pid:u32,pub helper_start_ticks:String,
    pub original_constraint:FinalizationConstraint,pub workload:serde_json::Value}
#[derive(Clone,Debug,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompilerSdkEnableData {pub mount:CompilerSdkMountData,#[serde(default,skip_serializing_if="Option::is_none")]pub probe:Option<CompilerSdkProbeData>}
struct Helper {listener:Option<OwnedFd>,name:String,child:Option<Child>,process:Option<Process>,start:Option<String>,
    endpoint:Option<RuntimeEndpoint>,peer:Option<OwnedFd>,accepted:Vec<OwnedFd>,pidfd:Option<OwnedFd>,pending:Option<RuntimePacket>,certificate:Option<OwnedFd>,fds:Vec<OwnedFd>,
    bytes:Option<Vec<u8>>,sent:bool,capsule_failed:bool,peer_witness:Option<peer_witness::OriginalSdkPeerImages>,result:Option<CompilerSdkProbeData>,exit:Option<ExitStatus>,attempted:bool,stderr:Option<File>,work:Option<File>,work_path:Option<PathBuf>}
impl Helper {fn staged()->Self{Self{listener:None,name:format!("rudder-sdk-q-{}",uuid::Uuid::new_v4()),child:None,process:None,start:None,endpoint:None,peer:None,accepted:Vec::new(),pidfd:None,pending:None,
    certificate:None,fds:Vec::new(),bytes:None,sent:false,capsule_failed:false,peer_witness:None,result:None,exit:None,attempted:false,stderr:None,work:None,work_path:None}}}
pub(super) struct RootCompilerSdkOwner {proof:Arc<RuntimeBootstrapProof>,admission:Arc<RuntimeReadAdmission>,original:FinalizationBudget,catalog:CompilerSdkCatalogData,
    placeholder:Option<File>,descriptors:Vec<OwnedFd>,source_binding:Option<File>,baked:Option<Arc<BakedImageCas>>,inputs:Option<Arc<VerifiedBakedInputs>>,semantic:Option<CompilerSdkSemanticInputs>,read:Option<VerifiedBakedRead>,
    local:Option<LocalStore>,floor:Option<ReadonlyRuntimeImage>,mount:Option<RetainedMetadataMount>,root:Option<OwnedFd>,data:Option<CompilerSdkMountData>,helper:Option<sdk_process::SdkProcess>,
    prepare_check:BootstrapCheck,ready:bool,failed:bool,failure:Option<anyhow::Error>}
impl RootCompilerSdkOwner {
    pub(super) fn capture_retaining(out:&mut Option<Self>,proof:Arc<RuntimeBootstrapProof>,admission:Arc<RuntimeReadAdmission>,original:FinalizationBudget,catalog:CompilerSdkCatalogData,qualification:bool)->Result<()> {
        ensure!(out.is_none(),"original SDK mount carrier occupied");
        original.remaining().map_err(|error|bootstrap_failure_context(error,BootstrapCheck::SdkPrepareOriginal))?;
        admission.current().map_err(|error|bootstrap_failure_context(error,BootstrapCheck::SdkPrepareOriginal))?;
        catalog.validate_data().map_err(|error|bootstrap_failure_context(error,BootstrapCheck::SdkCatalog))?;
        *out=Some(Self{proof,admission,original,catalog,
            placeholder:None,descriptors:Vec::new(),source_binding:None,baked:None,inputs:None,semantic:None,read:None,local:None,floor:None,mount:None,root:None,data:None,helper:None,prepare_check:BootstrapCheck::SdkPrepareOriginal,ready:false,failed:false,failure:None});
        let owner=out.as_mut().unwrap();if let Err(error)=owner.prepare(qualification){
            let diagnostic=retained_bootstrap_diagnostic(&error,owner.prepare_check);
            owner.failed=true;owner.failure=Some(error);return Err(diagnostic)}
        Ok(())
    }
    fn current_build(&self)->Result<()> {self.original.remaining()?;self.admission.current()?;self.proof.current()}
    fn prepare(&mut self,qualification:bool)->Result<()> {
        self.prepare_check=BootstrapCheck::SdkPrepareOriginal;self.current_build()?;
        self.prepare_check=BootstrapCheck::SdkPlaceholder;self.placeholder=Some(protected_directory(Path::new(SDK_SOURCE))?);
        let m=self.placeholder.as_ref().unwrap().metadata()?;ensure!(m.mode()&0o7777==0o555&&fs::read_dir(SDK_SOURCE)?.next().is_none(),"SDK DATA placeholder is not immutable empty image directory");
        self.prepare_check=BootstrapCheck::SdkBackingDescriptors;
        let sdk_cas=self.catalog.profile_data()?.cas_directory;
        let dir=protected_directory(Path::new(sdk_cas))?;
        let root=OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY|libc::O_NOFOLLOW|libc::O_CLOEXEC).open(Path::new(sdk_cas).join("cas"))?;
        self.descriptors.push(dir.into());self.descriptors.push(root.into());
        self.descriptors.push(OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW|libc::O_CLOEXEC).open(Path::new(sdk_cas).join("inventory.json"))?.into());
        self.prepare_check=BootstrapCheck::SdkSourceBinding;
        self.source_binding=Some(OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW|libc::O_CLOEXEC).open(Path::new(sdk_cas).join("compiler-source.json"))?);
        let source=self.source_binding.as_ref().unwrap();let m=source.metadata()?;ensure!(m.is_file()&&m.uid()==0&&m.gid()==0&&m.nlink()==1&&m.mode()&0o7777==0o444,"SDK producer binding owner differs");
        let raw=read_control(source,4096,&self.original)?;ensure!(digest(&raw)==self.catalog.source_binding_sha256,"SDK producer binding differs");
        let fence:Arc<dyn ImageImportFence>=Arc::new(CaptureFence(self.original.clone()));
        self.prepare_check=BootstrapCheck::SdkBakedInventory;
        capture_sdk_baked_retaining(&mut self.baked,&self.descriptors,self.catalog.inventory.clone(),fence.clone())?;
        let baked=self.baked.as_ref().unwrap();let inventory=baked.inventory_data()?;ensure!(inventory.roots.manifest==self.catalog.manifest&&inventory.roots.installer_input==self.catalog.installer_input,"SDK roots differ from original catalog");
        self.prepare_check=BootstrapCheck::SdkVerifiedManifest;
        VerifiedBakedInputs::capture_retaining(&mut self.inputs,baked.clone(),fence.clone())?;
        self.inputs.as_ref().unwrap().read_retaining(&mut self.read,self.catalog.manifest.clone(),fence.clone())?;
        let manifest:crate::image_preseed_data::ImageManifestData=serde_json::from_slice(self.read.as_ref().unwrap().verified_bytes()?)?;
        ensure!(manifest.format_version==1&&manifest.installer_input==self.catalog.installer_input,"SDK full manifest differs");
        self.prepare_check=BootstrapCheck::SdkSemanticInputs;
        CompilerSdkSemanticInputs::capture_retaining(&mut self.semantic,self.inputs.as_ref().unwrap().clone(),&self.catalog,&*fence)?;
        let semantic=self.semantic.as_ref().unwrap().data()?.clone();
        self.prepare_check=BootstrapCheck::SdkToolchains;
        let toolchains=catalog_derivation::derive_toolchains_data(self.inputs.as_ref().unwrap(),&self.catalog,&semantic,&manifest,&*fence)?;
        self.prepare_check=BootstrapCheck::SdkArtifactBinding;
        let original=self.inputs.as_ref().unwrap().compiler_file_content_data(&self.catalog,"manifest.json",&*fence)?;
        ensure!(original.content==self.catalog.artifact_manifest,"SDK installer lost actual original artifact manifest binding");
        self.prepare_check=BootstrapCheck::SdkStorage;
        self.read.take();self.current_build()?;fs::DirBuilder::new().mode(0o700).create(SDK_ROOT)?;
        self.local=Some(LocalStore::open(Path::new(SDK_ROOT).join("metadata-cache"))?);
        let restriction:Arc<dyn RuntimeImageReadRestriction>=self.admission.clone();
        self.prepare_check=BootstrapCheck::SdkReadFloor;
        ReadonlyRuntimeImage::capture_retaining(&mut self.floor,self.local.as_ref().unwrap().clone(),baked.clone(),restriction,fence)?;
        ensure!(self.floor.as_ref().unwrap().installer_root()?==&self.catalog.installer_input,"SDK floor changed complete closure");
        self.prepare_check=BootstrapCheck::SdkFuse;
        fs::DirBuilder::new().mode(0o700).create(SDK_FLOOR)?;
        RetainedMetadataMount::capture_retaining(&mut self.mount,RootMetadataNamespace::capture()?,self.floor.as_ref().unwrap().view()?,PathBuf::from(SDK_FLOOR),MetadataMountMode::ReadOnlyRuntimeData,uuid::Uuid::new_v4().to_string())?;
        self.mount.as_mut().unwrap().mount_retaining(&self.original,&AtomicBool::new(false))?;
        self.root=Some(self.mount.as_ref().unwrap().duplicate_runtime_root_retaining()?.0);
        self.prepare_check=BootstrapCheck::SdkMountData;
        self.data=Some(CompilerSdkMountData{catalog:self.catalog.clone(),semantic,floor:wire::fd_data(self.root.as_ref().unwrap())?,toolchains});self.data.as_ref().unwrap().validate_data()?;
        self.prepare_check=BootstrapCheck::SdkQualification;
        if qualification{sdk_process::SdkProcess::capture_retaining(&mut self.helper,self.proof.clone(),self.admission.clone(),self.original.clone(),self.root.as_ref().unwrap().try_clone()?,self.data.as_ref().unwrap().clone(),None,None,None)?;self.helper.as_mut().unwrap().run_retaining()?;}
        self.prepare_check=BootstrapCheck::SdkReady;self.current_build()?;self.ready=true;Ok(())
    }
    pub(super) fn current(&self)->Result<()> {ensure!(self.ready&&!self.failed,"original SDK floor incomplete/failed");self.admission.current()?;self.mount.as_ref().unwrap().check_current()?;self.inputs.as_ref().unwrap().binding_data()?;
        let held=self.placeholder.as_ref().context("original SDK placeholder absent")?.metadata()?;let named=fs::symlink_metadata(SDK_SOURCE)?;
        ensure!((held.dev(),held.ino(),held.mode(),held.uid(),held.gid())==(named.dev(),named.ino(),named.mode(),named.uid(),named.gid())&&held.is_dir()&&held.uid()==0&&held.gid()==0&&held.mode()&0o7777==0o555&&fs::read_dir(SDK_SOURCE)?.next().is_none(),"SDK original empty DATA placeholder changed");Ok(())}
    pub(super) fn enable_data(&self)->Result<CompilerSdkEnableData>{self.current()?;Ok(CompilerSdkEnableData{mount:self.data.as_ref().unwrap().clone(),probe:self.helper.as_ref().map(|h|h.result_data().cloned()).transpose()?})}
    pub(super) fn duplicate_floor(&self)->Result<OwnedFd>{self.current()?;Ok(self.root.as_ref().unwrap().try_clone()?)}
    pub(super) fn close_admission(&self){if let Some(m)=&self.mount{m.close_admission()}if let Some(f)=&self.floor{f.close_admission()}}

}
// BakedImageCas needs the original parent containing inventory.json and cas/.
// The separately retained cas descriptor at [1] remains in the SDK owner.
fn capture_sdk_baked_retaining(out:&mut Option<Arc<BakedImageCas>>,descriptors:&[OwnedFd],
    inventory:rudder_engine::protocol::ObjectRef,fence:Arc<dyn ImageImportFence>)->Result<()> {
    BakedImageCas::capture_retaining(out,descriptors[0].try_clone()?,descriptors[2].try_clone()?,inventory,fence)
}
fn abstract_address(name:&str)->Result<(libc::sockaddr_un,libc::socklen_t)>{ensure!(!name.is_empty()&&name.len()<=100&&!name.contains('\0'),"SDK abstract name invalid");let mut a:libc::sockaddr_un=unsafe{std::mem::zeroed()};a.sun_family=libc::AF_UNIX as _;for(i,b)in name.bytes().enumerate(){a.sun_path[i+1]=b as _;}let n=(std::mem::offset_of!(libc::sockaddr_un,sun_path)+1+name.len())as libc::socklen_t;Ok((a,n))}
fn peer_pidfd(fd:i32)->Result<OwnedFd>{let mut raw=-1;let mut len=std::mem::size_of::<i32>()as libc::socklen_t;ensure!(unsafe{libc::getsockopt(fd,libc::SOL_SOCKET,77,(&mut raw as *mut i32).cast(),&mut len)}==0&&raw>=0,"SDK kernel SO_PEERPIDFD unavailable");Ok(unsafe{OwnedFd::from_raw_fd(raw)})}
pub(super) fn validate_workload(value:&serde_json::Value,constraint:&FinalizationConstraint)->Result<()> {
    ensure!(value["format_version"]==1&&value["kind"]=="actual-fixed-rust-library-typescript-sdk-workload-v1"&&value["models"]==false&&value["source_authority"]==false&&value["normal_retirement_authority"]==false,"SDK actual workload shape differs");
    let start:u128=value["started_boottime_nanoseconds"].as_str().context("SDK workload start absent")?.parse()?;let end:u128=value["completed_boottime_nanoseconds"].as_str().context("SDK workload end absent")?.parse()?;
    let constraint_data=serde_json::to_value(constraint)?;let cutoff:u128=constraint_data["boottime_deadline_nanoseconds"].as_str().context("SDK original cutoff absent")?.parse()?;ensure!(start>0&&start<=end&&end<cutoff,"SDK workload left original enable clock");
    for name in ["buck2","rust","rust_result","typescript","typescript_result"]{ensure!(value[name]["exit_code"]==0,"SDK compiler invocation did not complete");}
    ensure!(value["buck2_static"]==true,"SDK Buck2 carries an undeclared interpreter");
    for key in ["CapEff","CapPrm","CapInh","CapAmb","CapBnd"]{ensure!(value["compiler_capabilities"][key].as_str().is_some_and(|s|s=="0000000000000000"),"SDK compiler retains namespace capabilities");}
    ensure!(value["rust_result"]["stdout_sha256"]==digest(b"")&&value["typescript_result"]["stdout_sha256"]==digest(b"rudder-sdk-typescript-ok\n"),"SDK actual compiled output differs");Ok(())
}

/// Pure DATA agreement with commands/environment actually passed to Popen by
/// the fixed runner. No process/result/grant is constructed by this check.
pub(in crate::root_storage_bootstrap) fn validate_selected_rules_workload(workload:&serde_json::Value,job:&serde_json::Value)->Result<()> {
    read_policy::validate_selected_confinement_data(workload)?;
    let rules:serde_json::Value=serde_json::from_str(include_str!("compiler_sdk_rules.json"))?;
    let targets=job["profile"]["targets"].as_array().context("SDK selected targets absent")?;let checks=workload["checks"].as_array().context("SDK actual compiler invocations absent")?;
    ensure!(targets.len()==checks.len(),"SDK actual compiler invocation count differs");
    for (target,check)in targets.iter().zip(checks){let language=target["language"].as_str().context("SDK selected language absent")?;
        let name=target["name"].as_str().context("SDK target name absent")?;let entrypoint=target["entrypoint"].as_str().context("SDK entrypoint absent")?;
        let directory=format!("{}/{}",rules["output_root"].as_str().context("SDK output rule absent")?,name);
        let(output,command)=if language=="rust"{(format!("{}/{}",directory,rules["rust_output"].as_str().context("SDK Rust output rule absent")?),"rust_argv")}else{ensure!(language=="typescript","SDK language unsupported");(directory,"typescript_argv")};
        let expected:Vec<String>=rules[command].as_array().context("SDK compiler rule absent")?.iter().map(|arg|->Result<String>{Ok(arg.as_str().context("SDK rule argument invalid")?.replace("{entrypoint}",entrypoint).replace("{output}",&output))}).collect::<Result<_>>()?;
        ensure!(check["command"]==serde_json::to_value(expected)?&&check["working_directory"]==rules["source_root"]&&check["environment"]==rules["environment"]&&check["target"]==target["name"]&&check["language"]==target["language"]&&check["exit_code"]==0,"SDK actual compiler invocation differs from semantic rules");
    }Ok(())
}

#[path="compiler_sdk_handshake_diagnostic.rs"]mod handshake_diagnostic;
#[path="compiler_sdk_process.rs"]mod sdk_process;
#[path="compiler_sdk_jobs.rs"]pub(super) mod jobs;
#[path="compiler_sdk_peer_witness.rs"]mod peer_witness;
#[path="compiler_sdk_helper.rs"]mod helper;
#[path="compiler_sdk_read_policy.rs"]mod read_policy;
pub fn helper_selected_early()->bool{std::env::args().nth(1).as_deref()==Some(HELPER_ARG)}
pub fn run_helper()->Result<()>{helper::run()}

#[cfg(test)]mod tests {
    use super::*;
    #[test]fn compiler_helper_cli_cannot_create_a_root_or_sdk_grant_from_ordinary_test_argv(){
        assert!(!helper_selected_early());assert!(run_helper().is_err());
    }
    #[test]fn semantic_rule_data_refuses_command_environment_or_working_directory_drift(){
        let rules:serde_json::Value=serde_json::from_str(include_str!("compiler_sdk_rules.json")).unwrap();let job=serde_json::json!({"profile":{"targets":[{"name":"lib","language":"rust","entrypoint":"lib.rs"}]}});
        let command:Vec<_>=rules["rust_argv"].as_array().unwrap().iter().map(|a|a.as_str().unwrap().replace("{entrypoint}","lib.rs").replace("{output}","/workspace/out/lib/library.rlib")).collect();
        let actual=serde_json::json!({"checks":[{"target":"lib","language":"rust","exit_code":0,"command":command,"environment":rules["environment"],"working_directory":rules["source_root"],"input_confinement":read_policy::child_read_observation_fixture_data()}]});
        validate_selected_rules_workload(&actual,&job).unwrap();
        for field in ["command","environment","working_directory"]{let mut changed=actual.clone();changed["checks"][0][field]=serde_json::json!("different");assert!(validate_selected_rules_workload(&changed,&job).is_err());}
    }
    #[test]fn sdk_fixed_workload_rejects_missing_data_capabilities_and_late_constraints(){
        let original=FinalizationBudget::begin(Duration::from_secs(30)).unwrap();
        assert!(validate_workload(&serde_json::json!({}),&original.constraint()).is_err());
        assert!(validate_workload(&serde_json::json!({"format_version":1,"kind":"actual-fixed-rust-library-typescript-sdk-workload-v1","models":false,"source_authority":false,"normal_retirement_authority":false,"started_boottime_nanoseconds":"1","completed_boottime_nanoseconds":serde_json::to_value(original.constraint()).unwrap()["boottime_deadline_nanoseconds"]}),&original.constraint()).is_err());
    }
}

#[cfg(test)]
#[path="compiler_sdk_baked_descriptor_tests.rs"]
mod baked_descriptor_tests;
