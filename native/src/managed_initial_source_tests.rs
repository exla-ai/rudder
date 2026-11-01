use super::*;
use crate::{engine_host::NativeAuthority, managed_task_launch::{ManagedTaskNetworkSelection,SelectedTaskCredentialKind}};
use jj_lib::config::{ConfigLayer,ConfigSource,StackedConfig};
use rudder_engine::{journal::ManagedProjectPrepare,protocol::ObjectKind,snapshot::EnvironmentV2,
    store::DirectoryManifest};
use std::{collections::BTreeMap,os::unix::fs::PermissionsExt,process::{Command,Stdio},time::Duration};

struct Fixture { root:PathBuf,source:ManagedInitialSourceOwner,engine:EngineOwnership,environment:ObjectRef,settings:UserSettings }
impl Fixture {
 fn new()->Self {
  let root=std::env::temp_dir().join(format!("rudder-owned-initial-source-{}",uuid::Uuid::new_v4()));
  fs::DirBuilder::new().mode(0o700).create(&root).unwrap();let repo=root.join("repo");let home=root.join("home");
  for p in [&repo,&home] {fs::DirBuilder::new().mode(0o700).create(p).unwrap();}
  let git=|args:&[&str]| {
   let output=Command::new("/usr/bin/git").args(args).current_dir(&repo).env_clear()
    .env("HOME",&home).env("GIT_CONFIG_NOSYSTEM","1").env("GIT_CONFIG_GLOBAL","/dev/null")
    .env("GIT_AUTHOR_NAME","Fixture").env("GIT_AUTHOR_EMAIL","fixture@invalid.example")
    .env("GIT_COMMITTER_NAME","Fixture").env("GIT_COMMITTER_EMAIL","fixture@invalid.example")
    .stdin(Stdio::null()).output().unwrap();assert!(output.status.success(),"fixture Git command failed");
  };
  git(&["init","--quiet","--initial-branch=main"]);fs::write(repo.join("tracked"),b"actual owned initial source\n").unwrap();
  git(&["add","--","tracked"]);git(&["-c","core.hooksPath=/dev/null","commit","--quiet","-m","initial"]);
  let mut engine=EngineOwnership::acquire_in_home(&repo,NativeAuthority::Fabric,&home).unwrap();
  let fence=engine.journal_parts().1.clone();let namespace=crate::managed_store::namespace(&home,&repo,&fence.project_id,engine.journal_parts().0).unwrap();
  engine.prepare_managed_project(&ManagedProjectPrepare {request_id:uuid::Uuid::new_v4().to_string(),namespace}).unwrap();
  let store=LocalStore::open(root.join("shared-cas")).unwrap();
  let empty=store.put_directory(DirectoryManifest{format_version:1,mode:0o755,entries:vec![]}).unwrap();
  let environment=store.put_metadata(ObjectKind::EnvironmentRoot,&EnvironmentV2{format_version:2,directory:empty,
   platform:BTreeMap::from([("fixture_abi".into(),"linux".into())]),installer_inputs:vec![],provenance:BTreeMap::new()}).unwrap();store.pin("environment",&environment).unwrap();
  let selected=ManagedTaskOptIn::selected("explicit-api".into(),crate::Backend::Codex,SelectedTaskCredentialKind::ApiKeyV1,ManagedTaskNetworkSelection::OrdinaryNetworkV1).unwrap();
  let source=ManagedInitialSourceOwner::capture(&mut engine,&selected,store).unwrap();
  let mut config=StackedConfig::with_defaults();let mut layer=ConfigLayer::empty(ConfigSource::User);
  layer.set_value("user.name","Fixture").unwrap();layer.set_value("user.email","fixture@invalid.example").unwrap();config.add_layer(layer);
  Self{root,source,engine,environment,settings:UserSettings::from_config(config).unwrap()}
 }
}
impl Drop for Fixture {fn drop(&mut self){let _=self.engine.finish(false);let _=fs::remove_dir_all(&self.root);}}
fn budget()->FinalizationBudget {FinalizationBudget::begin(Duration::from_secs(30)).unwrap()}

#[test]
fn actual_fabric_owner_and_held_writer_produce_the_journal_accepted_initial_source() {
 let mut f=Fixture::new();let original=budget();let cancel=AtomicBool::new(false);
 f.source.prepare(&f.environment,&f.settings,&original,&cancel).unwrap();
 let prepared=f.source.prepared_current(&original,&cancel).unwrap();
 assert_eq!(prepared.baseline.commit,f.source.head);assert_eq!(prepared.verified.receipt().source,prepared.source_root);
 assert_eq!(f.source.store.retention().unwrap().pins.get(&prepared.runnable_pin),Some(&prepared.runnable_root));
 assert_eq!(f.source.store.retention().unwrap().pins.get(&prepared.seed_pin),Some(&prepared.seed_root));
 {
  let import=f.source.git_import.try_lock().unwrap();
  let import=import.as_ref().expect("actual import carrier remains with its native source owner");
  assert!(import.progress().acknowledged);
  assert!(import.progress().index_installed);
 }
 assert!(f.source.prepare(&f.environment,&f.settings,&budget(),&cancel).is_err());
}
#[test]
fn cancelled_original_admission_never_creates_or_retries_integration_metadata() {
 let mut f=Fixture::new();let original=budget();original.cancel();let cancel=AtomicBool::new(false);
 assert!(f.source.prepare(&f.environment,&f.settings,&original,&cancel).is_err());
 assert!(!f.source.project.binding().directory.join("integration").exists());
 assert!(f.source.prepare(&f.environment,&f.settings,&budget(),&cancel).is_err());
}
#[test]
fn original_owner_retirement_refuses_initial_source_effects() {
 let mut f=Fixture::new();f.engine.finish(false).unwrap();
 assert!(f.source.prepare(&f.environment,&f.settings,&budget(),&AtomicBool::new(false)).is_err());
 assert!(!f.source.project.binding().directory.join("integration").exists());
}
#[test]
fn lost_final_ack_keeps_actual_prepared_owners_and_cannot_be_promoted_by_readback() {
 let mut f=Fixture::new();let original=budget();let cancel=AtomicBool::new(false);
 assert!(f.source.prepare_inner(&f.environment,&f.settings,&original,&cancel,||Err(anyhow::anyhow!("fixture lost final ACK"))).is_err());
 let prepared=f.source.prepared.as_ref().unwrap();
 assert_eq!(f.source.store.retention().unwrap().pins.get(&prepared.runnable_pin),Some(&prepared.runnable_root));
 assert!(!f.source.acknowledged);
 assert!(f.source.prepared_current(&budget(),&cancel).is_err());
 assert!(f.source.prepare(&f.environment,&f.settings,&budget(),&cancel).is_err());
}
#[test]
fn replaced_integration_directory_refuses_the_retained_result_without_unpinning() {
 let mut f=Fixture::new();let original=budget();let cancel=AtomicBool::new(false);
 f.source.prepare(&f.environment,&f.settings,&original,&cancel).unwrap();
 let path=f.source.integration.as_ref().unwrap().path.clone();let old=path.with_extension("preserved");
 fs::rename(&path,&old).unwrap();fs::create_dir(&path).unwrap();fs::set_permissions(&path,fs::Permissions::from_mode(0o700)).unwrap();
 assert!(f.source.prepared_current(&budget(),&cancel).is_err());
 let prepared=f.source.prepared.as_ref().unwrap();assert_eq!(f.source.store.retention().unwrap().pins.get(&prepared.seed_pin),Some(&prepared.seed_root));
}

#[test]
fn replaced_cas_or_managed_namespace_cannot_reconstruct_the_original_source_owner() {
 for replace_cas in [true,false] {
  let mut f=Fixture::new();let original=budget();let cancel=AtomicBool::new(false);
  f.source.prepare(&f.environment,&f.settings,&original,&cancel).unwrap();
  let path=if replace_cas {f.source.store.root().to_path_buf()} else {f.source.project.binding().directory.clone()};
  let old=path.with_extension("original-held");fs::rename(&path,&old).unwrap();
  fs::create_dir(&path).unwrap();fs::set_permissions(&path,fs::Permissions::from_mode(0o700)).unwrap();
  assert!(f.source.prepared_current(&budget(),&cancel).is_err());
  assert!(f.source.prepared.is_some());assert!(f.source.acknowledged);
 }
}

#[path="managed_initial_source_generation_tests.rs"]
mod generation_lifetime;
