use super::*;
use rudder_engine::{protocol::CheckpointConsistency,snapshot::{SourceV2,EnvironmentV2},store::{DirectoryManifest,DirectoryEntry,EntryNode}};
use std::{collections::BTreeMap,fs,os::unix::fs::DirBuilderExt};
struct Fixture{path:PathBuf,store:LocalStore}
impl Fixture{fn new()->Self{let path=std::env::temp_dir().join(format!("rudder-fresh-graph-{}",uuid::Uuid::new_v4()));fs::DirBuilder::new().mode(0o700).create(&path).unwrap();let store=LocalStore::open(path.join("cas")).unwrap();Self{path,store}}}
impl Drop for Fixture{fn drop(&mut self){let _=fs::remove_dir_all(&self.path);}}
fn roots(store:&LocalStore,source_dir:ObjectRef,installer:ObjectRef)->(ObjectRef,ObjectRef){let policy=store.put_blob(b"actual fixture policy DATA").unwrap();let source=store.put_metadata(ObjectKind::SourceRoot,&SourceV2{format_version:2,consistency:CheckpointConsistency::Quiesced,directory:source_dir.clone(),projection_policy:policy}).unwrap();let environment=store.put_metadata(ObjectKind::EnvironmentRoot,&EnvironmentV2{format_version:2,directory:source_dir,platform:BTreeMap::from([("os".into(),"linux".into())]),installer_inputs:vec![installer],provenance:BTreeMap::new()}).unwrap();(source,environment)}
#[test]
fn actual_complete_union_keeps_real_retained_image_graph_and_counts_only_new_project_objects(){
    let f=Fixture::new();let blob=f.store.put_blob(&vec![7u8;4*1024*1024]).unwrap();let image=f.store.put_directory(DirectoryManifest{format_version:1,mode:0o555,entries:vec![DirectoryEntry::new(b"image-file",EntryNode::File{object:blob,mode:0o444}).unwrap()]}).unwrap();f.store.pin("image-DATA-fixture",&image).unwrap();
    let known=f.store.retained_closure(&image).unwrap();let empty=f.store.put_directory(DirectoryManifest{format_version:1,mode:0o755,entries:vec![]}).unwrap();let (source,environment)=roots(&f.store,empty,image);let original=FinalizationBudget::begin(std::time::Duration::from_secs(5)).unwrap();let cancel=AtomicBool::new(false);
    let union=complete_union(&f.store,&source,&environment,&known,&original,&cancel).unwrap();assert!(union.windows(2).all(|w|w[0]<w[1]));for r in known{assert!(union.contains(&r));}assert!(union.contains(&source)&&union.contains(&environment));
    // An empty claimed membership forces normal verification/counting and
    // cannot fit these actual large image bytes into the small NEW profile.
    assert!(complete_union(&f.store,&source,&environment,&[],&original,&cancel).is_err());
    // This unit helper returns DATA only, never a FreshGenesisExport/Root ACK.
}
#[test]
fn new_project_oversized_object_and_original_cancel_are_refused(){
    let f=Fixture::new();let large=f.store.put_blob(&vec![0u8;1024*1024+1]).unwrap();let directory=f.store.put_directory(DirectoryManifest{format_version:1,mode:0o755,entries:vec![DirectoryEntry::new(b"large",EntryNode::File{object:large,mode:0o444}).unwrap()]}).unwrap();let empty=f.store.put_directory(DirectoryManifest{format_version:1,mode:0o555,entries:vec![]}).unwrap();f.store.pin("image-empty-DATA",&empty).unwrap();let image=f.store.retained_closure(&empty).unwrap();let (source,environment)=roots(&f.store,directory,empty);let original=FinalizationBudget::begin(std::time::Duration::from_secs(5)).unwrap();let cancel=AtomicBool::new(false);
    assert!(complete_union(&f.store,&source,&environment,&image,&original,&cancel).is_err());original.cancel();assert!(complete_union(&f.store,&source,&environment,&image,&original,&cancel).is_err());assert!(original.remaining().is_err());
}
