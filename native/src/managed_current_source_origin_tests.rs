//! Real local CAS/SQLite DATA checks. No Root client, SDK result or namespace
//! permission can be constructed by these fixtures.
use super::*;
use rudder_engine::store::{DirectoryManifest,DirectoryEntry,EntryNode};
struct Fixture {root:PathBuf,store:LocalStore}
impl Fixture {fn new()->Self{let root=std::env::temp_dir().join(format!("sdk-accepted-policy-{}",uuid::Uuid::new_v4()));fs::DirBuilder::new().mode(0o700).create(&root).unwrap();let store=LocalStore::open(root.join("cas")).unwrap();Self{root,store}}
 fn policy(&self,bytes:&[u8])->(ObjectRef,ObjectRef){let blob=self.store.put_blob(bytes).unwrap();let dir=self.store.put_directory(DirectoryManifest{format_version:1,mode:0o755,entries:vec![DirectoryEntry::new(b"rudder.toml",EntryNode::File{object:blob.clone(),mode:0o644}).unwrap()]}).unwrap();(dir,blob)}}
impl Drop for Fixture{fn drop(&mut self){let _=fs::remove_dir_all(&self.root);}}
fn clock()->FinalizationBudget{FinalizationBudget::begin(Duration::from_secs(30)).unwrap()}
fn profile()->Vec<u8>{format!(r#"format_version=1
name="accepted-smoke"
[platform]
os="linux"
architecture="x86_64"
abi="gnu-glibc-2.36"
[buck2]
release="actual-catalogue-selected-at-use"
executable_sha256="{}"
[policy]
version="selected-source-smoke-v1"
effects=[]
[[targets]]
name="library"
language="rust"
entrypoint="lib.rs"
sources=["lib.rs"]
[[targets]]
name="javascript"
language="typescript"
entrypoint="main.ts"
sources=["main.ts"]
"#,"a".repeat(64)).into_bytes()}
#[test]fn actual_cas_policy_reads_exact_accepted_rust_and_typescript_without_a_model_or_default(){let f=Fixture::new();let(dir,_)=f.policy(&profile());let p=read_accepted_profile(&f.store,&dir,&clock(),&AtomicBool::new(false)).unwrap();assert_eq!(p.targets.len(),2);assert!(p.policy.effects.is_empty());assert!(p.buck2.remote_cache.is_none());}
#[test]fn absent_symlink_oversized_or_cancelled_policy_never_selects_a_default(){let f=Fixture::new();let empty=f.store.put_directory(DirectoryManifest{format_version:1,mode:0o755,entries:vec![]}).unwrap();assert!(read_accepted_profile(&f.store,&empty,&clock(),&AtomicBool::new(false)).is_err());
 let link=f.store.put_directory(DirectoryManifest{format_version:1,mode:0o755,entries:vec![DirectoryEntry::new(b"rudder.toml",EntryNode::Symlink{target_hex:"666f726569676e".into()}).unwrap()]}).unwrap();assert!(read_accepted_profile(&f.store,&link,&clock(),&AtomicBool::new(false)).is_err());
 let(huge,_)=f.policy(&vec![b'x';128*1024+1]);assert!(read_accepted_profile(&f.store,&huge,&clock(),&AtomicBool::new(false)).is_err());let(dir,_)=f.policy(&profile());let original=clock();original.cancel();assert!(read_accepted_profile(&f.store,&dir,&original,&AtomicBool::new(false)).is_err());}
#[test]fn actual_cas_policy_hash_corruption_cannot_be_read_as_accepted_source(){let f=Fixture::new();let(dir,blob)=f.policy(&profile());let path=f.store.object_path(&blob).unwrap();use std::os::unix::fs::PermissionsExt;fs::set_permissions(&path,fs::Permissions::from_mode(0o644)).unwrap();fs::write(&path,vec![b'x';blob.size_bytes as usize]).unwrap();assert!(read_accepted_profile(&f.store,&dir,&clock(),&AtomicBool::new(false)).is_err());}
#[test]fn actual_journal_head_and_owner_changes_refuse_the_original_binding(){let f=Fixture::new();let mut journal=Journal::open(f.root.join("journal.sqlite")).unwrap();let owner=journal.create_project("project","engine","base",1).unwrap();let head=GitCommitId("base".into());check_head(&journal,&owner,&head).unwrap();
 let db=rusqlite::Connection::open(journal.database_path().unwrap()).unwrap();db.execute("UPDATE projects SET integration_head='next' WHERE project_id='project'",[]).unwrap();assert!(check_head(&journal,&owner,&head).is_err());db.execute("UPDATE projects SET integration_head='base' WHERE project_id='project'",[]).unwrap();check_head(&journal,&owner,&head).unwrap();journal.transfer_owner(&owner,"other",2).unwrap();assert!(check_head(&journal,&owner,&head).is_err());}
