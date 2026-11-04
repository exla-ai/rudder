//! Actual local Git/CAS/verification mechanics. No Native/Root/PG permission or
//! accepted-source grant is fabricated. All fixtures are disposable and private.
use super::*;
use crate::{projection::{project_source,verify_versioned_source},vfs_provider::SourceProjectionPolicy};
use jj_lib::{config::{ConfigLayer,ConfigSource,StackedConfig},object_id::ObjectId as _,repo::Repo,
    settings::UserSettings,workspace::Workspace};
use rudder_engine::{directory::{DirectoryEntry,EntryNode,LocalDirectoryStore,PagedDirectory},
    protocol::CheckpointConsistency,snapshot::SourceV2};
use std::{fs,os::unix::fs::{DirBuilderExt,MetadataExt,PermissionsExt},path::{Path,PathBuf},time::Duration};

struct Fixture {root:PathBuf,_workspace:Workspace,repo:Arc<jj_lib::repo::ReadonlyRepo>,cas:LocalStore}
impl Fixture {
    fn new()->Self {
        let base=if Path::new("/workspace").is_dir(){PathBuf::from("/workspace")}else{std::env::temp_dir()};
        let root=base.join(format!("rudder-git-reuse-{}",uuid::Uuid::new_v4()));
        fs::DirBuilder::new().mode(0o700).create(&root).unwrap();let integration=root.join("integration");
        fs::DirBuilder::new().mode(0o700).create(&integration).unwrap();
        let mut config=StackedConfig::with_defaults();let mut layer=ConfigLayer::empty(ConfigSource::User);
        layer.set_value("user.name","verified object fixture").unwrap();layer.set_value("user.email","verified@invalid.example").unwrap();config.add_layer(layer);
        let settings=UserSettings::from_config(config).unwrap();
        let(workspace,repo)=pollster::block_on(Workspace::init_internal_git(&settings,&integration)).unwrap();
        let cas=LocalStore::open(root.join("cas")).unwrap();Self{root,_workspace:workspace,repo,cas}
    }
    fn meter(&self,budget:ProjectionBudget)->MeteredSource{MeteredSource::new(self.cas.clone(),budget).unwrap()}
    fn object_path(&self,id:&FileId)->PathBuf{
        let hex=id.hex();git_backend(self.repo.store()).unwrap().git_repo_path().join("objects").join(&hex[..2]).join(&hex[2..])
    }
    fn overwrite_object(&self,id:&FileId,header:&[u8],payload:&[u8])->Vec<u8>{
        let mut compressed=gix::features::zlib::stream::deflate::Write::new(Vec::new());
        compressed.write_all(header).unwrap();compressed.write_all(payload).unwrap();compressed.flush().unwrap();let bytes=compressed.into_inner();
        let path=self.object_path(id);fs::set_permissions(&path,fs::Permissions::from_mode(0o644)).unwrap();fs::write(&path,&bytes).unwrap();
        fs::set_permissions(path,fs::Permissions::from_mode(0o444)).unwrap();bytes
    }
}
impl Drop for Fixture{fn drop(&mut self){let _=fs::remove_dir_all(&self.root);}}
fn stamp(path:&Path)->(u64,u64,u64,u32,(i64,i64),(i64,i64)){
    let m=fs::symlink_metadata(path).unwrap();assert!(m.is_file()&&!m.file_type().is_symlink());
    (m.dev(),m.ino(),m.len(),m.mode(),(m.mtime(),m.mtime_nsec()),(m.ctime(),m.ctime_nsec()))
}

#[test]
fn verified_git_reuse_cold_none_projection_and_full_verifier_preserve_loose_inode(){
    let f=Fixture::new();let payload=b"actual source must still be read and verified\n";
    let file=f.cas.put_blob(payload).unwrap();let stage=f.cas.begin_staging(Duration::from_secs(120)).unwrap();
    let adapter=LocalDirectoryStore::new(&f.cas,&stage.id);let source=PagedDirectory::new(&adapter).build(0o755,
        [DirectoryEntry::new(b"tracked",EntryNode::File{object:file.clone(),logical_size:payload.len()as u64,mode:0o644}).unwrap()]).unwrap();
    let policy=f.cas.put_blob(&rudder_engine::protocol::canonical_json(&SourceProjectionPolicy::from_import_policy(
        &rudder_engine::workspace::WorkspaceProfile::source_only().source_exclusions()).unwrap()).unwrap()).unwrap();
    let baseline=f.repo.store().get_commit(f.repo.view().get_wc_commit_id(jj_lib::ref_name::WorkspaceName::DEFAULT).unwrap()).unwrap();
    let first=pollster::block_on(project_source(f.cas.clone(),&source,&baseline.tree(),&policy,None,None,ProjectionBudget::default())).unwrap();
    let oid=gix::objs::compute_hash(gix::hash::Kind::Sha1,gix::objs::Kind::Blob,payload).unwrap();let id=FileId::new(oid.as_bytes().to_vec());
    let path=f.object_path(&id);let original=stamp(&path);
    let again=pollster::block_on(project_source(f.cas.clone(),&source,&first.tree,&policy,None,None,ProjectionBudget::default())).unwrap();
    assert_eq!(again.work.reused_leaf_values,0,"cold None cannot assert prior mapping equality");
    assert!(again.work.cas_content_bytes>=payload.len()as u64);assert!(again.work.git_blob_bytes_submitted>=payload.len()as u64);
    assert_eq!(stamp(&path),original,"full input verification must preserve identical stored object");
    let source_root=f.cas.put_metadata(ObjectKind::SourceRoot,&SourceV2{format_version:2,consistency:CheckpointConsistency::Quiesced,
        directory:source,projection_policy:policy.clone()}).unwrap();
    let verified=pollster::block_on(verify_versioned_source(f.cas.clone(),&stage.id,&source_root,&policy,&again.tree,None,ProjectionBudget::default())).unwrap();
    assert_eq!(verified.receipt().work.reused_leaf_values,0);assert!(verified.receipt().work.cas_content_bytes>=payload.len()as u64);
    assert_eq!(stamp(&path),original,"mandatory verifier must also avoid loose-object replacement");
    let pin_count=f.cas.retention().unwrap().pins.len();let cas_path=f.cas.object_path(&file).unwrap();
    fs::set_permissions(&cas_path,fs::Permissions::from_mode(0o644)).unwrap();fs::write(&cas_path,vec![b'!';payload.len()]).unwrap();
    fs::set_permissions(cas_path,fs::Permissions::from_mode(0o444)).unwrap();
    assert!(pollster::block_on(verify_versioned_source(f.cas.clone(),&stage.id,&source_root,&policy,&again.tree,None,ProjectionBudget::default())).is_err());
    assert_eq!(stamp(&path),original);assert_eq!(f.cas.retention().unwrap().pins.len(),pin_count,"corrupt input cannot issue a new mapping proof");
}

#[test]
fn verified_git_reuse_checks_actual_stored_bytes_and_never_repairs_corruption(){
    let f=Fixture::new();let payload=b"same verified input";let file=f.cas.put_blob(payload).unwrap();
    let first=write_file(&f.meter(ProjectionBudget::default()),f.repo.store(),&file,payload.len()as u64).unwrap();let path=f.object_path(&first);
    let wrong=vec![b'?';payload.len()];let original=f.overwrite_object(&first,format!("blob {}\0",payload.len()).as_bytes(),&wrong);let before=stamp(&path);
    assert!(write_file(&f.meter(ProjectionBudget::default()),f.repo.store(),&file,payload.len()as u64).is_err());
    assert_eq!(fs::read(&path).unwrap(),original);assert_eq!(stamp(&path),before,"failed verification must not replace/repair original corrupted object");
}

#[test]
fn verified_git_reuse_refuses_wrong_oversize_and_corrupt_headers_without_repair(){
    for variant in 0..3 {
        let f=Fixture::new();let payload=b"bounded input";let file=f.cas.put_blob(payload).unwrap();
        let id=write_file(&f.meter(ProjectionBudget::default()),f.repo.store(),&file,payload.len()as u64).unwrap();let path=f.object_path(&id);
        let bytes=match variant {
            0=>f.overwrite_object(&id,format!("tree {}\0",payload.len()).as_bytes(),payload),
            // Header preflight MUST refuse before a full find/allocation.
            1=>f.overwrite_object(&id,b"blob 1099511627776\0",b"tiny"),
            _=>{let bytes=b"invalid compressed Git object".to_vec();fs::set_permissions(&path,fs::Permissions::from_mode(0o644)).unwrap();fs::write(&path,&bytes).unwrap();fs::set_permissions(&path,fs::Permissions::from_mode(0o444)).unwrap();bytes},
        };let before=stamp(&path);
        assert!(write_file(&f.meter(ProjectionBudget::default()),f.repo.store(),&file,payload.len()as u64).is_err());
        assert_eq!(fs::read(&path).unwrap(),bytes);assert_eq!(stamp(&path),before);
    }
}

#[test]
fn verified_git_reuse_capacity_refusal_keeps_legacy_streaming_budget(){
    for max_buffer in [0,1024] {
        let f=Fixture::new();let payload=b"actual stream";let file=f.cas.put_blob(payload).unwrap();
        // Aggregate capacity cannot cover TWO buffers in either case, but the
        // original single streaming source read still fits its unchanged quota.
        let budget=ProjectionBudget{max_jj_buffer_bytes:max_buffer,max_content_bytes:payload.len()as u64,..ProjectionBudget::default()};let source=f.meter(budget);
        let id=write_file(&source,f.repo.store(),&file,payload.len()as u64).unwrap();assert!(f.object_path(&id).is_file());
        assert_eq!(source.work().jj_buffer_bytes_admitted,0);assert_eq!(source.work().cas_content_bytes,payload.len()as u64);
    }
}

#[test]
fn verified_git_reuse_expired_meter_and_bad_cas_are_errors_not_fallback_writes(){
    let f=Fixture::new();let payload=b"read failure retains old inode";let file=f.cas.put_blob(payload).unwrap();
    let id=write_file(&f.meter(ProjectionBudget::default()),f.repo.store(),&file,payload.len()as u64).unwrap();let path=f.object_path(&id);let original=stamp(&path);
    let source=f.meter(ProjectionBudget::default());source.meter.lock().unwrap().started=Instant::now()-Duration::from_secs(31);
    assert!(write_file(&source,f.repo.store(),&file,payload.len()as u64).is_err());assert_eq!(stamp(&path),original);
    let cas_path=f.cas.object_path(&file).unwrap();fs::set_permissions(&cas_path,fs::Permissions::from_mode(0o644)).unwrap();fs::write(&cas_path,vec![b'!';payload.len()]).unwrap();fs::set_permissions(&cas_path,fs::Permissions::from_mode(0o444)).unwrap();
    assert!(write_file(&f.meter(ProjectionBudget::default()),f.repo.store(),&file,payload.len()as u64).is_err());assert_eq!(stamp(&path),original);
}
