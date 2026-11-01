//! Explicit Linux/UID0 storage+jJ COMPONENT. Fixture DATA is supplied by the
//! disposable runner; no runtime mount, project, task, PG or SDK grant is made.
#![cfg(all(feature="jj",target_os="linux"))]
mod common;
#[path="environment_dependency_packed_history/fifty_forks.rs"]
mod fifty_forks;
use anyhow::{ensure,Context,Result};
use rudder_engine::{build::{BuildIndex,RetainedBackedPreparation,PreparationSpec,PreparedContentLayout,PlatformIdentity,ToolchainIdentity,index_acquisition::RetainedIndexAcquisition},
    execution::finalization_budget::FinalizationBudget,protocol::{ObjectKind,ObjectRef},
    store::{LocalStore,ImageImportFence,baked_image::BakedImageCas,backed_inputs::VerifiedBakedInputs,environment_dependencies::{RetainedEnvironmentSubgraph,EnvironmentSubgraphRead}}};
use rudder_vcs_bridge::{GitCommitId,initial_source::{InitialSourceRequest,PreparedInitialSource,backed::{BackedInitialEnvironment,prepare_initial_source_backed_history_retaining}},seed::{SeedProfile,PagedSeedGeneration},accepted_git_import::{RetainedGitImport,GitHistoryProfile}};
use std::{collections::{BTreeMap,BTreeSet},fs::{self,OpenOptions},os::{fd::OwnedFd,unix::fs::{DirBuilderExt,OpenOptionsExt}},path::{Path,PathBuf},sync::{Arc,atomic::AtomicBool},time::{Duration,Instant}};
struct Clock(FinalizationBudget);
impl Clock{fn new()->Self{Self(FinalizationBudget::begin(Duration::from_secs(30)).unwrap())}}
impl ImageImportFence for Clock{fn remaining(&self)->Result<Duration>{self.0.remaining()}}
fn fd(path:&Path,dir:bool)->Result<OwnedFd>{Ok(OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW|libc::O_CLOEXEC|libc::O_NONBLOCK|if dir{libc::O_DIRECTORY}else{0}).open(path)?.into())}
/// Staged before effects; intentionally process-lifetime retained even if the
/// test errors/panics. Runner destroys its disposable VM, never calls this drain.
#[derive(Default)]struct Owners{baked:Option<Arc<BakedImageCas>>,inputs:Option<Arc<VerifiedBakedInputs>>,local:Option<LocalStore>,index_open:Option<RetainedIndexAcquisition>,index:Option<Arc<BuildIndex>>,
    preparation:Option<RetainedBackedPreparation>,environment:Option<Arc<RetainedEnvironmentSubgraph>>,read:Option<EnvironmentSubgraphRead>,
    context:Option<BackedInitialEnvironment>,import:Option<RetainedGitImport>,paged:Option<PagedSeedGeneration>,source:Option<PreparedInitialSource>}
fn counts(store:&LocalStore)->Result<(u64,u64)>{let mut count=0;let mut bytes=0;for shard in fs::read_dir(store.root().join("objects"))?{let shard=shard?;ensure!(shard.file_type()?.is_dir(),"non-directory CAS shard");for file in fs::read_dir(shard.path())?{let file=file?;ensure!(file.file_type()?.is_file(),"non-file CAS object");count+=1;bytes+=file.metadata()?.len();}}Ok((count,bytes))}
#[test]
#[ignore="actual full Rudder history plus immutable baked runtime; Root-owned DATA component only"]
fn full_rudder_packed_history_source_seed_and_runtime_use_one_retained_environment_boundary()->Result<()> {
    ensure!(unsafe{libc::geteuid()}==0,"strict Root-owned baked DATA fixture requires UID0");
    let run_fifty=match std::env::var("RUDDER_DEPENDENCY_FIFTY_FORKS") {
        Err(std::env::VarError::NotPresent)=>false,
        Ok(value)=>{ensure!(value=="1","fifty-fork component requires exact explicit selector1");true},
        Err(error)=>return Err(error.into()),
    };
    let inventory_text=std::env::var("RUDDER_DEPENDENCY_IMAGE_INVENTORY")?;ensure!(inventory_text.len()<=512,"inventory DATA bound");let expected:ObjectRef=serde_json::from_str(&inventory_text)?;expected.validate()?;
    let source_git=PathBuf::from(std::env::var("RUDDER_DEPENDENCY_SOURCE_GIT")?);ensure!(source_git.is_absolute()&&source_git.as_os_str().len()<=4096,"source test DATA bound");
    let head=GitCommitId(std::env::var("RUDDER_DEPENDENCY_SOURCE_HEAD")?);head.validate()?;
    let output=PathBuf::from(std::env::var("RUDDER_DEPENDENCY_OUTPUT")?);ensure!(output.starts_with("/workspace")&&!output.exists(),"new disposable /workspace output required");fs::DirBuilder::new().mode(0o700).create(&output)?;
    let held=Box::leak(Box::<Owners>::default());let mut timings=BTreeMap::new();let capture=Arc::new(Clock::new());let start=Instant::now();
    let root=Path::new("/var/lib/rudder-image-cas-v1");BakedImageCas::capture_retaining(&mut held.baked,fd(root,true)?,fd(&root.join("inventory.json"),false)?,expected.clone(),capture.clone())?;
    let baked=held.baked.as_ref().unwrap().clone();let inventory=baked.inventory_data()?;
    VerifiedBakedInputs::capture_retaining(&mut held.inputs,baked.clone(),capture.clone())?;capture.remaining()?;timings.insert("typed_capture_ms",start.elapsed().as_millis());
    let inputs=held.inputs.as_ref().unwrap().clone();let binding=inputs.binding_data()?;ensure!(binding.encoded_bytes>1500*1024*1024&&binding.object_count>10_000,"fixture lacks real full runtime scale");
    let mut manifest_read=None;baked.read_manifest_retaining(&mut manifest_read,capture.clone())?;let manifest:serde_json::Value=serde_json::from_slice(manifest_read.as_ref().unwrap().verified_bytes()?)?;
    let native=manifest["files"].as_array().context("complete manifest absent")?.iter().find(|f|f["path"]=="/opt/rudder/dist/native/rudder-native").context("actual native content absent")?;
    let executable:ObjectRef=serde_json::from_value(native["content"].clone())?;let sha=native["sha256"].as_str().context("actual native hash absent")?;capture.remaining()?;
    held.local=Some(LocalStore::open(output.join("cas"))?);let store=held.local.as_ref().unwrap().clone();let original=Clock::new();let cancelled=Arc::new(AtomicBool::new(false));
    BuildIndex::create_shared_retaining(&mut held.index_open,output.join("index"),store.clone(),original.0.clone(),cancelled.clone(),Arc::new(original))?;held.index=Some(held.index_open.as_ref().unwrap().index()?.clone());
    let prep_clock=Clock::new();let start=Instant::now();let network=store.put_blob(b"declared storage-component-only VM input\n")?;store.pin("component-network",&network)?;
    let spec=PreparationSpec{format_version:1,base_runtime:inventory.roots.installer_input.clone(),platform:PlatformIdentity{os:"linux".into(),architecture:std::env::consts::ARCH.into(),abi:"storage-component-only".into(),properties:BTreeMap::new()},
        installer_inputs:BTreeMap::from([("image/tree".into(),inventory.roots.installer_input.clone()),("image/manifest".into(),inventory.roots.manifest.clone()),("vm/network".into(),network.clone())]),
        installer_commands:Vec::new(),installer_environment:BTreeMap::new(),toolchains:BTreeMap::from([("runtime-native-data".into(),ToolchainIdentity{version:format!("sha256:{sha}"),closure:inventory.roots.installer_input.clone(),executable,executable_path:"package/dist/native/rudder-native".into(),configuration:BTreeMap::new()})]),policy_version:"actual-rudder-environment-boundary-component-v1".into(),effects:BTreeSet::new()};
    held.index.as_ref().unwrap().prepare_backed_retaining(&mut held.preparation,spec,PreparedContentLayout::PagedChunkedV2,inventory.roots.installer_input.clone(),vec![network],vec![inputs.clone()],None,prep_clock.0.clone(),cancelled.clone())?;
    prep_clock.remaining()?;timings.insert("environment_prepare_ms",start.elapsed().as_millis());
    // Completion is immutable: no re-creation of its old clock on derivation.
    prep_clock.0.cancel();let derive=Arc::new(Clock::new());let start=Instant::now();held.preparation.as_ref().unwrap().derive_environment_subgraph_retaining(&mut held.environment,derive.clone())?;derive.remaining()?;timings.insert("environment_derive_ms",start.elapsed().as_millis());
    let environment=held.environment.as_ref().unwrap().clone();let environment_root=environment.root_data()?.clone();
    let private=output.join("private");fs::DirBuilder::new().mode(0o700).create(&private)?;let seed=output.join("seed");let original=Clock::new();let constraint=original.0.constraint();let start=Instant::now();
    BackedInitialEnvironment::stage_subgraph_retaining(&mut held.context,environment.clone(),"actual-rudder-runnable".into(),original.0.clone(),cancelled.clone())?;
    let source_result=pollster::block_on(prepare_initial_source_backed_history_retaining(store.clone(),InitialSourceRequest{source_git:&source_git,private_repository:&private,seed_destination:&seed,expected_head:&head,environment:&environment_root,settings:&common::settings()},
        SeedProfile::PagedCowV2,GitHistoryProfile::PackedHistoryV2,&mut held.paged,&mut held.import,held.context.as_mut().unwrap(),&original.0,cancelled.clone(),||{original.remaining()?;Ok(())}));
    if let Err(error)=&source_result{if let Some(actual)=&held.import{eprintln!("RUDDER_PACKED_HISTORY_FAILURE_DATA={}",serde_json::json!({"progress":actual.progress(),"packed":actual.packed_progress_data()}));}return Err(anyhow::anyhow!("{error:#}"));}
    held.source=Some(source_result?);
    original.remaining()?;ensure!(original.0.constraint()==constraint,"initial source clock replaced");timings.insert("source_seed_runnable_ms",start.elapsed().as_millis());
    let source=held.source.as_ref().unwrap();ensure!(source.baseline.commit==head&&source.baseline.parents.len()>0,"actual accepted history changed/truncated");
    let owner=source.runnable_dependency().context("genuine dependent runnable owner absent")?;ensure!(Arc::ptr_eq(owner.environment(),&environment)&&source.runnable_backing().is_none(),"source flattened/fabricated another boundary");
    let history=held.import.as_ref().context("actual packed import owner absent")?.packed_data()?;
    ensure!(history.profile=="packed_history_v2"&&history.reconstructed_bytes>384*1024*1024&&history.objects>=12_000
        &&history.encoded_bytes<=384*1024*1024&&history.verification_work_bytes<=1024*1024*1024&&history.canonical_child_reaped_and_all_pipes_eof,"actual packed history/profile/work proof differs");
    let work=held.paged.as_ref().unwrap().work();ensure!(work.read_files>=3&&work.read_bytes>=history.encoded_bytes,"seed did not retain complete original compressed history");
    let source_bytes=owner.record_data().local_bytes;ensure!(source_bytes<=384*1024*1024&&owner.record_data().local_objects<=100_000&&owner.record_data().depth==2,"source limits changed");
    let combined=binding.encoded_bytes.checked_add(source_bytes).and_then(|n|n.checked_add(work.read_bytes)).context("combined size overflow")?;// Compressed history deliberately avoids a combined raw-history quota; each actual boundary remains independently retained.
    let before=counts(&store)?;let read_clock=Arc::new(Clock::new());environment.read_retaining(&mut held.read,environment_root,read_clock)?;held.read.as_ref().unwrap().verified_bytes()?;ensure!(counts(&store)?==before,"retained image read copied payload locally");
    let state=store.retention()?;ensure!(!state.pins.values().any(|r|r==&source.runnable_root)&&owner.matches_record_data(&state,&source.runnable_pin),"foreign graph became an ordinary LocalPin");
    let result=serde_json::json!({"formatVersion":1,"passed":true,"scope":"storage-jj-component-only","inventory":expected,"head":head,"runtimeEncodedBytes":binding.encoded_bytes,
        "sourceUniqueEncodedBytes":source_bytes,"seedReadBytes":work.read_bytes,"seedReadFiles":work.read_files,"combinedObservedBytes":combined,"environmentDependencyDepth":2,"timingsMs":timings,
        "localObjects":before.0,"localEncodedBytes":before.1,"perOperationOriginalDeadlineMs":30000,"historyProfile":history,"actualAgentLaunch":false,"RootMountAuthority":false,"normalDrain":false,"compilerSDK":false});
    fs::write(output.join("result.json"),serde_json::to_vec_pretty(&result)?)?;println!("RUDDER_ENVIRONMENT_DEPENDENCY_PACKED_HISTORY_V2={result}");
    if run_fifty {fifty_forks::run(&output,&store,source,environment.clone(),inputs.clone())?;}
    Ok(())
}
