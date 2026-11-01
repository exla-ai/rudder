//! Actual local CAS/index preparation DATA fixtures. No Root or registration
//! owner is fabricated; custody labels deliberately stay outside the recipe.
use super::*;
use rudder_engine::store::DirectoryManifest;
use std::os::unix::fs::DirBuilderExt;
struct Fixture{root:PathBuf,index:Arc<BuildIndex>,spec:PreparationSpec}
impl Fixture{fn new()->Self{let base=if Path::new("/workspace").is_dir(){PathBuf::from("/workspace")}else{std::env::temp_dir()};let root=base.join(format!("rudder-runtime-content-{}",uuid::Uuid::new_v4()));fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
    let store=LocalStore::open(root.join("cas")).unwrap();let tool=store.put_blob(b"actual runtime tool content DATA").unwrap();let image=store.put_directory(DirectoryManifest{format_version:1,mode:0o555,entries:vec![rudder_engine::store::DirectoryEntry::new(b"runtime-tool",rudder_engine::store::EntryNode::File{object:tool.clone(),mode:0o555}).unwrap()]}).unwrap();
    let manifest=store.put_blob(b"actual runtime closure fixture DATA").unwrap();let network=store.put_blob(b"actual VM network A\n").unwrap();
    for(name,r)in [("image",&image),("manifest",&manifest),("network",&network)]{store.pin(name,r).unwrap();}
    let spec=PreparationSpec{format_version:1,base_runtime:image.clone(),platform:runtime_platform("runtime-component-v1"),
        installer_inputs:BTreeMap::from([("image/installer".into(),image.clone()),("image/manifest".into(),manifest),("vm/network".into(),network)]),
        toolchains:BTreeMap::from([("runtime-tool".into(),ToolchainIdentity{version:"component-data-v1".into(),closure:image,executable:tool,executable_path:"runtime-tool".into(),configuration:BTreeMap::new()})]),installer_commands:Vec::new(),installer_environment:BTreeMap::new(),policy_version:"runtime-component-v1".into(),effects:BTreeSet::new()};
    Self{index:Arc::new(BuildIndex::open(root.join("index"),store).unwrap()),root,spec}}
    fn prepare(&self,spec:PreparationSpec)->rudder_engine::build::RetainedPreparation{let mut out=None;let anchors=spec.installer_inputs.values().cloned().collect();
        self.index.prepare_pinned_retaining(&mut out,spec.clone(),PreparedContentLayout::PagedChunkedV2,spec.base_runtime.clone(),anchors,FinalizationBudget::begin(Duration::from_secs(30)).unwrap(),Arc::new(AtomicBool::new(false))).unwrap();out.unwrap()}}
impl Drop for Fixture{fn drop(&mut self){let _=fs::remove_dir_all(&self.root);}}
#[test]fn identical_runtime_content_reuses_actual_environment_across_distinct_custody_data(){let f=Fixture::new();
    let first_custody=("engine-A","config-A","image-receipt-A");let second_custody=("engine-B","config-B","image-receipt-B");assert_ne!(first_custody,second_custody);
    let first=f.prepare(f.spec.clone());let second=f.prepare(f.spec.clone());
    assert_eq!(first.acknowledgment().unwrap().prepared(),second.acknowledgment().unwrap().prepared());assert!(second.acknowledgment().unwrap().reused());
    let env=rudder_engine::snapshot::decode_environment(&f.index.store().read_object(&first.acknowledgment().unwrap().prepared().root).unwrap()).unwrap();
    for label in ["engine_instance_id","config_sha256","image","image_receipt_sha256"]{assert!(!env.platform.contains_key(label));}}
#[test]fn declared_runtime_network_content_changes_actual_environment_identity(){let f=Fixture::new();let first=f.prepare(f.spec.clone());let mut changed=f.spec.clone();
    let network=f.index.store().put_blob(b"actual VM network B\n").unwrap();f.index.store().pin("network-B",&network).unwrap();changed.installer_inputs.insert("vm/network".into(),network);
    let second=f.prepare(changed);assert_ne!(first.acknowledgment().unwrap().prepared().root,second.acknowledgment().unwrap().prepared().root);}
