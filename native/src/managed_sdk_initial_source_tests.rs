//! Constructed SDK DATA + actual local jj/Git effects. No Root/source/SDK grant.
//! Definitions only: the private author has not compiled or executed these.
use super::*;
use rudder_engine::{build::{PlatformIdentity,ToolchainIdentity,BuildProfile},protocol::{ObjectRef,ObjectKind}};
fn spec()->PreparationSpec {
    let root=ObjectRef::from_bytes(ObjectKind::Directory,b"constructed directory metadata DATA");
    PreparationSpec{format_version:1,platform:PlatformIdentity{os:"linux".into(),architecture:"x86_64".into(),abi:"gnu".into(),properties:BTreeMap::new()},base_runtime:root.clone(),
        installer_inputs:BTreeMap::new(),installer_commands:vec![],installer_environment:BTreeMap::new(),policy_version:"constructed-sdk-data".into(),effects:Default::default(),
        toolchains:BTreeMap::from([("buck2".into(),ToolchainIdentity{version:"explicit-sdk-version".into(),closure:root,executable:ObjectRef::from_bytes(ObjectKind::Blob,b"constructed executable DATA"),executable_path:"prepared/buck2".into(),configuration:BTreeMap::from([("executable_raw_sha256".into(),"a".repeat(64))])})])}
}
#[test]
fn fixed_source_policies_use_exact_original_platform_frontend_and_positive_or_denied_inputs(){
    let sdk=spec();let files=files_data(&sdk).unwrap();assert_eq!(files.len(),7);
    for name in ["rudder.toml","sdk-probes/proc.toml","sdk-probes/job.toml"]{
        let profile=BuildProfile::from_toml(std::str::from_utf8(&files[name]).unwrap()).unwrap();
        assert_eq!(profile.platform,sdk.platform);assert_eq!(profile.buck2.release,sdk.toolchains["buck2"].version);
        assert_eq!(profile.buck2.executable_sha256,sdk.toolchains["buck2"].configuration["executable_raw_sha256"]);
        assert!(profile.buck2.remote_cache.is_none()&&profile.policy.effects.is_empty());
    }
    assert!(std::str::from_utf8(&files["sdk-probes/proc.rs"]).unwrap().contains("/proc/sys/kernel/random/uuid"));
    assert!(std::str::from_utf8(&files["sdk-probes/job.rs"]).unwrap().contains("/workspace/job.json"));
    assert_eq!(BuildProfile::from_toml(std::str::from_utf8(&files["rudder.toml"]).unwrap()).unwrap().targets.len(),2);
    assert!(files.values().map(Vec::len).sum::<usize>()<=32*1024);
}
#[test]
fn fixed_source_never_substitutes_missing_frontend_or_discards_actual_platform_properties(){
    let mut sdk=spec();sdk.toolchains.remove("buck2");assert!(files_data(&sdk).is_err());
    let mut sdk=spec();sdk.toolchains.get_mut("buck2").unwrap().configuration.clear();assert!(files_data(&sdk).is_err());
    let mut sdk=spec();sdk.toolchains.get_mut("buck2").unwrap().configuration.insert("executable_raw_sha256".into(),"A".repeat(64));assert!(files_data(&sdk).is_err());
    let mut sdk=spec();sdk.platform.properties.insert("required".into(),"actual-value".into());assert!(files_data(&sdk).is_err());
}
struct Fixture(PathBuf);
impl Fixture{fn new()->Self{let raw=std::env::temp_dir().join(format!("rudder-sdk-source-data-{}",uuid::Uuid::new_v4()));fs::create_dir(&raw).unwrap();Self(raw.canonicalize().unwrap())}}
impl Drop for Fixture{fn drop(&mut self){let _=fs::remove_dir_all(&self.0);}}
fn settings()->jj_lib::settings::UserSettings{use jj_lib::config::{StackedConfig,ConfigLayer,ConfigSource};let mut c=StackedConfig::with_defaults();let mut l=ConfigLayer::empty(ConfigSource::User);l.set_value("user.name","Fixed source DATA fixture").unwrap();l.set_value("user.email","fixture@invalid.example").unwrap();c.add_layer(l);jj_lib::settings::UserSettings::from_config(c).unwrap()}
#[test]
fn actual_first_jj_head_and_git_tree_include_every_fixed_file_before_any_project_journal(){
    let f=Fixture::new();let files=files_data(&spec()).unwrap();let original=FinalizationBudget::begin(std::time::Duration::from_secs(20)).unwrap();let cancel=AtomicBool::new(false);
    let(mut workspace,mut repo,mut head)=(None,None,None);
    super::super::initialize_git_files_retaining(&mut workspace,&mut repo,&mut head,&f.0,&settings(),&original,&cancel,Some(&files)).unwrap();
    let repo=repo.as_ref().unwrap();let head=head.as_ref().unwrap();let commit=repo.store().get_commit(repo.view().get_wc_commit_id(WorkspaceName::DEFAULT).unwrap()).unwrap();assert_eq!(commit.id().hex(),head.0);
    let git=repo.store().backend_impl::<GitBackend>().unwrap().git_repo_path();assert_eq!(fs::read_to_string(git.join("HEAD")).unwrap(),format!("{}\n",head.0));
    let git=repo.store().backend_impl::<GitBackend>().unwrap().git_repo();let commit=git.head().unwrap().id().unwrap().object().unwrap().try_into_commit().unwrap();let tree=commit.tree().unwrap();
    for(name,bytes)in files{let actual=tree.lookup_entry_by_path(name).unwrap().unwrap().object().unwrap().try_into_blob().unwrap().take_data();assert_eq!(actual,bytes);}
    assert!(!f.0.join(".rudder/engine/journal.sqlite").exists());
}
#[test]
fn cancellation_precedes_first_repository_effect_and_does_not_create_a_replacement_clock(){
    let f=Fixture::new();let original=FinalizationBudget::begin(std::time::Duration::from_secs(20)).unwrap();let queued=original.clone();original.cancel();let cancel=AtomicBool::new(false);
    let(mut workspace,mut repo,mut head)=(None,None,None);let files=files_data(&spec()).unwrap();
    assert!(super::super::initialize_git_files_retaining(&mut workspace,&mut repo,&mut head,&f.0,&settings(),&queued,&cancel,Some(&files)).is_err());
    assert!(workspace.is_none()&&repo.is_none()&&head.is_none());assert!(!f.0.join(".jj").exists());assert_eq!(queued.constraint(),original.constraint());
}
