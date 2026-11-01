//! Real owned Git/jj stores. No mocks substitute for reader/sync/install effects.
use super::*;
use crate::{accepted_git_import::{import_expected_git_head_retaining,RetainedGitImport,GitImportLimits},GitCommitId};
use gix::objs::Write as _;
use jj_lib::{git_backend::GitBackend,repo::Repo,workspace::Workspace,config::{StackedConfig,ConfigLayer,ConfigSource},settings::UserSettings};
use std::{sync::Arc,time::Duration};
fn settings()->UserSettings{let mut config=StackedConfig::with_defaults();let mut layer=ConfigLayer::empty(ConfigSource::User);layer.set_value("user.name","Owned pack test").unwrap();layer.set_value("user.email","pack@invalid.example").unwrap();config.add_layer(layer);UserSettings::from_config(config).unwrap()}
fn clock()->FinalizationBudget{FinalizationBudget::begin(Duration::from_secs(30)).unwrap()}
fn raw(repo:&gix::Repository,kind:Kind,b:&[u8])->ObjectId{repo.objects.write_stream(kind,b.len()as u64,&mut &b[..]).unwrap()}
fn commit(repo:&gix::Repository,tree:ObjectId,parent:Option<ObjectId>,message:&str)->ObjectId{let mut b=format!("tree {tree}\n");if let Some(p)=parent{b.push_str(&format!("parent {p}\n"));}b.push_str(&format!("author A <a@invalid.example> 1 +0000\ncommitter A <a@invalid.example> 1 +0000\n\n{message}\n"));raw(repo,Kind::Commit,b.as_bytes())}
fn target(path:&Path)->(Workspace,Arc<jj_lib::repo::ReadonlyRepo>){fs::create_dir(path).unwrap();pollster::block_on(Workspace::init_internal_git(&settings(),path)).unwrap()}
struct Fixture{_root:tempfile::TempDir,root:PathBuf,source:gix::Repository,workspace:Workspace,target:Arc<jj_lib::repo::ReadonlyRepo>,head:ObjectId,objects:Vec<ObjectId>}
impl Fixture{
 fn new()->Self{let temp=tempfile::tempdir().unwrap();let root=temp.path().canonicalize().unwrap();let source=gix::init_bare(root.join("source.git")).unwrap();let blob=raw(&source,Kind::Blob,b"actual immutable history\n");let link=raw(&source,Kind::Blob,b"tracked");let mut tree=Vec::new();for(mode,name,id)in[("120000","link",link),("100755","tracked",blob)]{tree.extend_from_slice(format!("{mode} {name}\0").as_bytes());tree.extend_from_slice(id.as_bytes());}let tree=raw(&source,Kind::Tree,&tree);let parent=commit(&source,tree,None,"parent");let head=commit(&source,tree,Some(parent),"accepted");fs::write(source.path().join("HEAD"),format!("{head}\n")).unwrap();let(workspace,target)=target(&root.join("target"));Self{_root:temp,root,source,workspace,target,head,objects:vec![blob,link,tree,parent,head]}}
 fn backend(&self)->&GitBackend{self.target.store().backend_impl::<GitBackend>().unwrap()}
 fn pack_dir(&self)->PathBuf{self.backend().git_repo_path().join("objects/pack")}
 fn run(&self,out:&mut Option<RetainedGitImport>,original:&FinalizationBudget,cancelled:Arc<AtomicBool>,guard:impl FnMut()->Result<()>)->Result<crate::accepted_git_import::ImportedGitHeadData>{import_expected_git_head_retaining(out,&self.source,self.backend(),&GitCommitId(self.head.to_string()),GitImportLimits::default(),original,cancelled,guard)}
 fn files(&self)->Vec<PathBuf>{let entries=match fs::read_dir(self.pack_dir()){Ok(v)=>v,Err(e)if e.kind()==std::io::ErrorKind::NotFound=>return Vec::new(),Err(e)=>panic!("read owned pack directory: {e}")};let mut result=entries.map(|p|p.unwrap().path()).collect::<Vec<_>>();result.sort();result}
}
fn check_pack_and_index(path:&Path){
 let index=gix::odb::pack::index::File::at(path,gix::hash::Kind::Sha1).unwrap();let raw=fs::read(path).unwrap();let mut hash=gix::hash::hasher(gix::hash::Kind::Sha1);hash.update(&raw[..raw.len()-20]);assert_eq!(hash.try_finalize().unwrap(),index.index_checksum());
 let bytes=fs::read(path.with_extension("pack")).unwrap();assert!(bytes.len()as u64<=MAX_PACK_BYTES);let mut hash=gix::hash::hasher(gix::hash::Kind::Sha1);hash.update(&bytes[..bytes.len()-20]);assert_eq!(hash.try_finalize().unwrap(),index.pack_checksum());assert_eq!(&bytes[bytes.len()-20..],index.pack_checksum().as_slice());
 let mut entries=index.iter().collect::<Vec<_>>();entries.sort_by_key(|e|e.pack_offset);for(i,e)in entries.iter().enumerate(){let end=entries.get(i+1).map_or(bytes.len()-20,|n|n.pack_offset as usize);assert_eq!(Some(gix::features::hash::crc32(&bytes[e.pack_offset as usize..end])),e.crc32);}
}
#[test]
fn complete_history_is_self_contained_verified_pack_and_survives_source_object_removal(){
 let f=Fixture::new();let expected=f.objects.iter().map(|id|{let obj=f.source.find_object(*id).unwrap();(*id,obj.kind,obj.data.to_vec())}).collect::<Vec<_>>();let before=pollster::block_on(f.target.loader().op_heads_store().get_op_heads()).unwrap();let mut owner=None;let result=f.run(&mut owner,&clock(),Arc::new(AtomicBool::new(false)),||Ok(())).unwrap();assert_eq!(result.objects,5);
 let p=owner.as_ref().unwrap().progress();assert!(p.acknowledged&&p.pack_synced&&p.index_synced&&p.keep_synced&&p.pack_installed&&p.index_installed&&p.keep_installed);assert!(p.directory_syncs>=8);assert_eq!(p.pack_count,1);
 for path in f.files(){assert!(!path.to_string_lossy().ends_with("partial"));if path.extension().is_some_and(|s|s=="idx"){check_pack_and_index(&path);}assert_eq!(fs::metadata(path).unwrap().permissions().mode()&0o777,0o444);}
 fs::remove_dir_all(f.source.path().join("objects")).unwrap();fs::create_dir(f.source.path().join("objects")).unwrap();let actual=gix::open(f.backend().git_repo_path()).unwrap();for(id,kind,bytes)in expected{let value=actual.find_object(id).unwrap();assert_eq!(value.kind,kind);assert_eq!(&value.data[..],&bytes);assert_eq!(gix::objs::compute_hash(gix::hash::Kind::Sha1,kind,&value.data).unwrap(),id);}
 assert!(!actual.path().join("objects/info/alternates").exists());assert_eq!(pollster::block_on(f.target.loader().op_heads_store().get_op_heads()).unwrap(),before);assert!(f.target.store().get_commit(&jj_lib::backend::CommitId::new(f.head.as_bytes().to_vec())).is_ok());assert!(f.files().iter().any(|p|p.extension().is_some_and(|e|e=="keep")));
}
#[test]
fn packed_source_is_resolved_into_another_independent_pack_without_external_bases(){
 let f=Fixture::new();let mut first=None;f.run(&mut first,&clock(),Arc::new(AtomicBool::new(false)),||Ok(())).unwrap();fs::write(f.backend().git_repo_path().join("HEAD"),format!("{}\n",f.head)).unwrap();let packed=gix::open(f.backend().git_repo_path()).unwrap();let(_workspace,repo)=target(&f.root.join("second"));let backend=repo.store().backend_impl::<GitBackend>().unwrap();let mut second=None;
 let result=import_expected_git_head_retaining(&mut second,&packed,backend,&GitCommitId(f.head.to_string()),GitImportLimits::default(),&clock(),Arc::new(AtomicBool::new(false)),||Ok(())).unwrap();assert_eq!(result.objects,5);let actual=gix::open(backend.git_repo_path()).unwrap();for id in &f.objects{assert_eq!(actual.find_object(*id).unwrap().data,packed.find_object(*id).unwrap().data);}
}
#[test]
fn original_cancellation_before_effects_retains_no_ack_and_no_target_pack_files(){
 let f=Fixture::new();let original=clock();let cancelled=Arc::new(AtomicBool::new(true));let before=f.files();let mut owner=None;let error=f.run(&mut owner,&original,cancelled,||Ok(())).unwrap_err();assert!(format!("{error:#}").contains("cancelled"));let progress=owner.as_ref().unwrap().progress();assert!(!progress.acknowledged&&progress.pack_count==0);assert_eq!(f.files(),before);assert!(original.remaining().is_err());
}
#[test]
fn actual_late_cancellation_keeps_durable_index_owner_and_never_acknowledges_or_replays(){
 let f=Fixture::new();let original=clock();let cancel=Arc::new(AtomicBool::new(false));let mut owner=None;
 let error=f.run(&mut owner,&original,cancel.clone(),||{if f.files().iter().any(|p|p.extension().is_some_and(|s|s=="idx")){cancel.store(true,Ordering::Release);anyhow::bail!("actual late cancellation after published index");}Ok(())}).unwrap_err();assert!(format!("{error:#}").contains("actual late cancellation"));let state=owner.as_ref().unwrap().progress();assert!(!state.acknowledged&&state.index_installed&&state.pack_installed&&state.pack_synced&&state.index_synced);assert!(owner.as_ref().unwrap().data().is_err());assert!(state.error_chain.as_ref().unwrap().contains("actual late cancellation"));let before=f.files();assert!(f.run(&mut owner,&original,cancel,||Ok(())).is_err());assert_eq!(f.files(),before);
}
#[test]
fn failure_after_pack_publication_retains_orphan_and_gc_keep_without_visible_index(){
 let f=Fixture::new();let mut owner=None;let error=f.run(&mut owner,&clock(),Arc::new(AtomicBool::new(false)),||{if f.files().iter().any(|p|p.extension().is_some_and(|s|s=="pack")){anyhow::bail!("owned failure before index publication");}Ok(())}).unwrap_err();assert!(format!("{error:#}").contains("before index"));let p=owner.as_ref().unwrap().progress();assert!(p.pack_installed&&p.keep_installed&&!p.index_installed&&!p.acknowledged);assert!(f.files().iter().any(|p|p.to_string_lossy().ends_with("idx.partial")));assert!(gix::open(f.backend().git_repo_path()).unwrap().find_object(f.head).is_err());
}
#[test]
fn corrupted_source_and_changed_head_never_publish_an_index(){
 let f=Fixture::new();let id=f.objects[0].to_string();let path=f.source.path().join("objects").join(&id[..2]).join(&id[2..]);fs::set_permissions(&path,fs::Permissions::from_mode(0o600)).unwrap();fs::write(path,b"corrupt zlib object").unwrap();let mut owner=None;assert!(f.run(&mut owner,&clock(),Arc::new(AtomicBool::new(false)),||Ok(())).is_err());assert!(!owner.as_ref().unwrap().progress().acknowledged);assert!(!f.files().iter().any(|p|p.extension().is_some_and(|s|s=="idx")));assert!(owner.as_ref().unwrap().progress().error_chain.is_some());
 let f=Fixture::new();let mut owner=None;assert!(f.run(&mut owner,&clock(),Arc::new(AtomicBool::new(false)),||{if f.files().iter().any(|p|p.extension().is_some_and(|s|s=="pack")){fs::write(f.source.path().join("HEAD"),format!("{}\n",f.objects[3]))?;}Ok(())}).is_err());assert!(!owner.as_ref().unwrap().progress().acknowledged);assert!(!f.files().iter().any(|p|p.extension().is_some_and(|s|s=="idx")));
}
#[test]
fn unwind_retains_the_actual_initialized_pack_files_in_the_callers_carrier(){
 let f=Fixture::new();let mut owner=None;let result=std::panic::catch_unwind(std::panic::AssertUnwindSafe(||{let _=f.run(&mut owner,&clock(),Arc::new(AtomicBool::new(false)),||{if f.files().iter().any(|p|p.to_string_lossy().ends_with("partial")){panic!("owned unwind after pack capture");}Ok(())});}));assert!(result.is_err());let owner=owner.as_ref().unwrap();assert!(!owner.progress().acknowledged);assert_eq!(owner.progress().temporary_paths.iter().filter(|p|p.exists()).count(),3);
}
#[test]
#[ignore="explicit ~70MiB owned /workspace fixture; real shard-boundary and reader check"]
fn incompressible_history_is_sharded_below64mib_without_loose_object_copies(){
 let temp=tempfile::tempdir_in("/workspace").unwrap();let root=temp.path().canonicalize().unwrap();let source=gix::init_bare(root.join("source.git")).unwrap();let mut state=0x817ac3e6f029134bu64;let mut ids=Vec::new();for _ in 0..3{let mut b=vec![0;24<<20];for byte in &mut b{state^=state<<13;state^=state>>7;state^=state<<17;*byte=state as u8;}ids.push(raw(&source,Kind::Blob,&b));}
 let mut entries=Vec::new();for(i,id)in ids.iter().enumerate(){entries.extend_from_slice(format!("100644 blob-{i}\0").as_bytes());entries.extend_from_slice(id.as_bytes());}let tree=raw(&source,Kind::Tree,&entries);let head=commit(&source,tree,None,"real shard fixture");fs::write(source.path().join("HEAD"),format!("{head}\n")).unwrap();let(_workspace,repo)=target(&root.join("target"));let backend=repo.store().backend_impl::<GitBackend>().unwrap();let mut owner=None;import_expected_git_head_retaining(&mut owner,&source,backend,&GitCommitId(head.to_string()),GitImportLimits::default(),&clock(),Arc::new(AtomicBool::new(false)),||Ok(())).unwrap();assert!(owner.as_ref().unwrap().progress().pack_count>=2);
 for p in owner.as_ref().unwrap().progress().installed_paths{if p.extension().is_some_and(|s|s=="pack"){assert!(fs::metadata(p).unwrap().len()<=MAX_PACK_BYTES);}else if p.extension().is_some_and(|s|s=="idx"){check_pack_and_index(&p);}}
 let actual=gix::open(backend.git_repo_path()).unwrap();for id in ids{let value=actual.find_object(id).unwrap();assert_eq!(gix::objs::compute_hash(gix::hash::Kind::Sha1,value.kind,&value.data).unwrap(),id);}
}
