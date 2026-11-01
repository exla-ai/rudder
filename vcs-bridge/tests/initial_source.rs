#![cfg(feature = "jj")]
mod common;
use gix::{objs::{Kind, Write as _}, ObjectId};
use jj_lib::{object_id::ObjectId as _, op_store::RefTarget, ref_name::RefName};
use rudder_engine::{execution::finalization_budget::FinalizationBudget, protocol::{CheckpointConsistency, ObjectKind},
    snapshot::{decode_runnable, decode_source, EnvironmentV2}, store::{LocalStore, DirectoryManifest}};
use rudder_vcs_bridge::{GitCommitId, initial_source::*, publication::INTEGRATION_BOOKMARK};
use std::{collections::BTreeMap, fs, os::unix::fs::PermissionsExt, path::PathBuf, time::Duration};

struct Fixture { _temp: tempfile::TempDir, source: gix::Repository, store: LocalStore,
    private: PathBuf, seed: PathBuf, head: GitCommitId, environment: rudder_engine::protocol::ObjectRef }
impl Fixture {
 fn new() -> Self {
    let temp = tempfile::tempdir().unwrap();
    let source = gix::init_bare(temp.path().join("source.git")).unwrap();
    let raw = |kind, bytes: &[u8]| source.objects.write_stream(kind, bytes.len() as u64, &mut &bytes[..]).unwrap();
    let blob = raw(Kind::Blob, b"exact accepted bytes $Format:%H$\n");
    let link = raw(Kind::Blob, b"tracked");
    let attrs = raw(Kind::Blob, b"tracked export-ignore export-subst\n");
    let mut entries = Vec::new();
    for (mode,name,id) in [("100644",".gitattributes",attrs),("120000","link",link),("100755","tracked",blob)] {
        entries.extend_from_slice(format!("{mode} {name}\0").as_bytes()); entries.extend_from_slice(id.as_bytes());
    }
    let tree = raw(Kind::Tree,&entries);
    let make_commit = |parent: Option<ObjectId>| {
        let mut bytes = format!("tree {tree}\n");
        if let Some(id) = parent { bytes.push_str(&format!("parent {id}\n")); }
        bytes.push_str("author A <a@invalid.example> 1 +0000\ncommitter A <a@invalid.example> 1 +0000\n\naccepted immutable source\n");
        raw(Kind::Commit,bytes.as_bytes())
    };
    let parent = make_commit(None); let head = make_commit(Some(parent));
    fs::write(source.path().join("HEAD"),format!("{head}\n")).unwrap();
    let store = LocalStore::open(temp.path().join("cas")).unwrap();
    let empty = store.put_directory(DirectoryManifest {format_version:1,mode:0o755,entries:vec![]}).unwrap();
    let environment = store.put_metadata(ObjectKind::EnvironmentRoot,&EnvironmentV2 {format_version:2,directory:empty,
        platform:BTreeMap::from([("fixture_abi".into(),"linux-root-independent".into())]),installer_inputs:vec![],provenance:BTreeMap::new()}).unwrap();
    store.pin("environment",&environment).unwrap();
    let private = temp.path().join("private");fs::create_dir(&private).unwrap();fs::set_permissions(&private,fs::Permissions::from_mode(0o700)).unwrap();
    let seed=temp.path().join("seed");
    Self {_temp:temp,source,store,private,seed,head:GitCommitId(head.to_string()),environment}
 }
 fn run(&self,budget:&FinalizationBudget,owner:impl FnMut()->anyhow::Result<()>) -> anyhow::Result<PreparedInitialSource> {
    pollster::block_on(prepare_initial_source(self.store.clone(),InitialSourceRequest {source_git:self.source.path(),
      private_repository:&self.private,seed_destination:&self.seed,expected_head:&self.head,
      environment:&self.environment,settings:&common::settings()},budget,owner))
 }
}
fn budget()->FinalizationBudget {FinalizationBudget::begin(Duration::from_secs(30)).unwrap()}

#[test]
fn actual_git_history_jj_bookmark_source_bytes_modes_and_seed_form_one_baseline() {
 let f=Fixture::new();let prepared=f.run(&budget(),||Ok(())).unwrap();
 assert_eq!(prepared.baseline.commit,f.head);
 let repo=prepared.repository();
 assert_eq!(repo.view().get_local_bookmark(RefName::new(INTEGRATION_BOOKMARK)),&RefTarget::normal(
    jj_lib::backend::CommitId::new(hex::decode(&f.head.0).unwrap())));
 assert_eq!(prepared.operation.0,repo.op_id().hex());
 assert_eq!(prepared.baseline.parents.len(),1);
 let runnable=decode_runnable(&f.store.read_object(&prepared.runnable_root).unwrap()).unwrap();
 assert_eq!(runnable.environment,Some(f.environment.clone())); assert_eq!(runnable.source,prepared.source_root);
 let source=decode_source(&f.store.read_object(&runnable.source).unwrap()).unwrap(); assert_eq!(source.directory,runnable.view_root);
 assert_eq!(prepared.verified.receipt().source,prepared.source_root);
 assert_eq!(prepared.verified.receipt().tree,prepared.baseline.tree);
 let exported=f._temp.path().join("source-bytes");rudder_engine::tree::export_quiesced(&f.store,&rudder_engine::tree::TreeSnapshot {format_version:1,directory:source.directory.clone(),hardlinks:runnable.hardlinks.clone()},&exported,&rudder_engine::tree::ImportBudget::default()).unwrap();
 assert_eq!(fs::read(exported.join("tracked")).unwrap(),b"exact accepted bytes $Format:%H$\n");
 assert_eq!(fs::metadata(exported.join("tracked")).unwrap().permissions().mode()&0o111,0o111);
 assert_eq!(fs::read_link(exported.join("link")).unwrap(),PathBuf::from("tracked"));
 assert!(!exported.join(".git").exists());assert!(!exported.join(".jj").exists());
 let retention=f.store.retention().unwrap(); assert_eq!(retention.pins.get(&prepared.runnable_pin),Some(&prepared.runnable_root));
 assert_eq!(retention.pins.get(&prepared.seed_pin),Some(&prepared.seed_root));
 assert_eq!(f.store.import_tree(&f.seed,CheckpointConsistency::Quiesced).unwrap(),prepared.seed_root);
 let manifest:rudder_vcs_bridge::seed::MetadataTemplate=serde_json::from_slice(&fs::read(f.seed.join("manifest.json")).unwrap()).unwrap();
 assert_eq!(manifest.baseline,prepared.baseline);assert_eq!(manifest.operation,prepared.operation);
 assert_eq!(fs::read(f.seed.join("metadata/.git/HEAD")).unwrap(),format!("{}\n",f.head.0).as_bytes());
 assert_eq!(gix::open(f.private.join(".jj/repo/store/git")).unwrap().head().unwrap().id().unwrap().detach().to_string(),f.head.0);
 assert_eq!(fs::read(f.source.path().join("HEAD")).unwrap(),format!("{}\n",f.head.0).as_bytes());
 // Failure does not retry/overwrite a completed target or acquire new authority.
 assert!(f.run(&budget(),||Ok(())).is_err());
}

#[test]
fn cancelled_original_budget_has_no_repository_or_seed_effects() {
 let f=Fixture::new();let original=budget();original.cancel();assert!(f.run(&original,||Ok(())).is_err());
 assert!(!f.private.join(".jj").exists());assert!(!f.seed.exists());
}

#[test]
fn unretained_environment_is_refused_before_private_repository_initialization() {
 let f=Fixture::new();f.store.unpin("environment").unwrap();assert!(f.run(&budget(),||Ok(())).is_err());
 assert!(!f.private.join(".jj").exists());assert!(!f.seed.exists());
}

#[test]
fn wrong_head_preserves_partial_private_target_and_never_publishes_a_runnable_root() {
 let mut f=Fixture::new();f.head=GitCommitId("a".repeat(40));assert!(f.run(&budget(),||Ok(())).is_err());
 assert!(f.private.join(".jj").is_dir());assert!(!f.seed.exists());
 assert_eq!(f.store.retention().unwrap().pins.len(),1);
 assert!(f.run(&budget(),||Ok(())).is_err());
}

#[test]
fn actual_owner_loss_after_initialization_cannot_renew_or_complete_preparation() {
 let f=Fixture::new();let original=budget();let mut checks=0;
 assert!(f.run(&original,||{checks+=1;if checks==3 {original.cancel();} Ok(())}).is_err());
 assert!(f.private.join(".jj").is_dir());assert!(!f.seed.exists());assert!(original.remaining().is_err());
 assert_eq!(f.store.retention().unwrap().pins.len(),1);
}

#[test]
fn seed_destinations_inside_private_or_source_metadata_and_parent_aliases_are_refused() {
 for variant in 0..3 {
  let mut f=Fixture::new();
  f.seed=match variant {
   0=>f.private.join("seed"),
   1=>f.source.path().join("seed"),
   _=>{let alias=f._temp.path().join("private-alias");std::os::unix::fs::symlink(&f.private,&alias).unwrap();alias.join("seed")}
  };
  assert!(f.run(&budget(),||Ok(())).is_err());
  assert!(!f.private.join(".jj").exists());assert!(!f.seed.exists());
  assert_eq!(f.store.retention().unwrap().pins.len(),1);
 }
}
