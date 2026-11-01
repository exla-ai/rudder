//! Actual local CAS/SQLite policy selection. No SDK, Root or model grant.
use super::*;
use rudder_engine::{journal::{CapabilityGrant,DecisionRequest},store::{DirectoryEntry,DirectoryManifest,EntryNode as LegacyEntry}};
use std::{os::unix::fs::DirBuilderExt,path::Path};
struct Fixture{path:PathBuf,store:LocalStore,journal:Journal,owner:OwnerFence,token:String}
impl Fixture{fn new()->Self{let base=if Path::new("/workspace").is_dir(){Path::new("/workspace")}else{Path::new("/tmp")};let path=base.join(format!("canonical-policy-{}",uuid::Uuid::new_v4()));
    fs::DirBuilder::new().mode(0o700).create(&path).unwrap();let store=LocalStore::open(path.join("cas")).unwrap();let mut journal=Journal::open(path.join("journal.sqlite")).unwrap();
    let owner=journal.create_project("project","engine","base",1).unwrap();journal.register_run(&owner,"run",1).unwrap();let grant=CapabilityGrant{principal:"policy-author".into(),run_id:"run".into(),run_epoch:1,
        operations:BTreeSet::from(["decision:accept".into()]),recipients:BTreeMap::new(),scopes:BTreeSet::from(["build-policy".into()]),topics:BTreeSet::from([rudder_engine::journal::CANONICAL_BUILD_POLICY_TOPIC.into()]),expires_at_ms:None};
    let token=journal.issue_capability(&owner,&grant,1).unwrap();Self{path,store,journal,owner,token}}
    fn clock()->FinalizationBudget{FinalizationBudget::begin(Duration::from_secs(30)).unwrap()}
    fn directory(&self,bytes:&[u8])->ObjectRef{let blob=self.store.put_blob(bytes).unwrap();self.store.put_directory(DirectoryManifest{format_version:1,mode:0o755,entries:vec![
        DirectoryEntry::new(b"rudder.toml",LegacyEntry::File{object:blob,mode:0o644}).unwrap()]}).unwrap()}
    fn file(&self,bytes:&[u8])->PolicyFile{policy_file(&self.store,&self.directory(bytes),&Self::clock(),&AtomicBool::new(false)).unwrap()}
    fn snapshot(&self)->rudder_engine::journal::CanonicalDecisionSnapshot{self.journal.capture_canonical_decisions(&self.owner,"base",&Self::clock(),&AtomicBool::new(false)).unwrap()}
    fn authorize(&mut self,base:&PolicyFile,next:&PolicyFile,head:&str){self.journal.accept_decision(&self.token,&DecisionRequest{idempotency_key:uuid::Uuid::new_v4().to_string(),topic:rudder_engine::journal::CANONICAL_BUILD_POLICY_TOPIC.into(),expected_version:0,scope:"build-policy".into(),
        statement:serde_json::json!({"format_version":1,"schema":"canonical-gates-policy-v1","expected_head":head,"previous_profile":base.reference,"next_profile":next.reference})},2).unwrap();}
}
impl Drop for Fixture{fn drop(&mut self){let _=fs::remove_dir_all(&self.path);}}
fn raw(name:&str,source:&str)->Vec<u8>{format!(r#"format_version=1
name="{name}"
[platform]
os="linux"
architecture="x86_64"
abi="component-sdk"
[buck2]
release="component"
executable_sha256="{}"
[policy]
version="canonical-component-v1"
effects=[]
[[targets]]
name="library"
language="rust"
entrypoint="{source}"
sources=["{source}"]
"#,"a".repeat(64)).into_bytes()}
#[test]fn canonical_candidate_cannot_disable_accepted_gates_by_editing_rudder_toml(){let f=Fixture::new();let base=f.file(&raw("accepted","lib.rs"));let next=f.file(&raw("candidate","other.rs"));
    assert!(select_profiles(&base,&next,&f.snapshot()).is_err());let mut semantic_same=raw("accepted","lib.rs");semantic_same.extend_from_slice(b"\n# candidate formatting only\n");
    let same=f.file(&semantic_same);assert_ne!(base.reference,same.reference);assert_eq!(select_profiles(&base,&same,&f.snapshot()).unwrap(),vec![base.profile]);}
#[test]fn canonical_actual_accepted_policy_change_runs_both_old_and_new_profiles(){let mut f=Fixture::new();let base=f.file(&raw("accepted","lib.rs"));let next=f.file(&raw("approved","other.rs"));
    f.authorize(&base,&next,"base");let selected=select_profiles(&base,&next,&f.snapshot()).unwrap();assert_eq!(selected,vec![base.profile.clone(),next.profile.clone()]);
    let other=f.file(&raw("unapproved","third.rs"));assert!(select_profiles(&base,&other,&f.snapshot()).is_err());}
#[test]fn canonical_accepted_policy_authorization_binds_original_head_and_exact_bytes(){let mut f=Fixture::new();let base=f.file(&raw("accepted","lib.rs"));let next=f.file(&raw("approved","other.rs"));
    f.authorize(&base,&next,"different-base");assert!(select_profiles(&base,&next,&f.snapshot()).is_err());}
#[test]fn canonical_policy_absent_oversized_symlink_or_cancelled_refuses(){let f=Fixture::new();let empty=f.store.put_directory(DirectoryManifest{format_version:1,mode:0o755,entries:vec![]}).unwrap();
    assert!(policy_file(&f.store,&empty,&Fixture::clock(),&AtomicBool::new(false)).is_err());let huge=f.directory(&vec![b'x';(MAX_POLICY+1)as usize]);assert!(policy_file(&f.store,&huge,&Fixture::clock(),&AtomicBool::new(false)).is_err());
    let link=f.store.put_directory(DirectoryManifest{format_version:1,mode:0o755,entries:vec![DirectoryEntry::new(b"rudder.toml",LegacyEntry::Symlink{target_hex:"656c73657768657265".into()}).unwrap()]}).unwrap();
    assert!(policy_file(&f.store,&link,&Fixture::clock(),&AtomicBool::new(false)).is_err());let dir=f.directory(&raw("accepted","lib.rs"));let clock=Fixture::clock();clock.cancel();assert!(policy_file(&f.store,&dir,&clock,&AtomicBool::new(false)).is_err());}
#[test]fn canonical_changed_code_outside_all_accepted_targets_refuses_but_docs_are_not_claimed_compiled(){let f=Fixture::new();let policy=f.file(&raw("accepted","lib.rs"));let entry=FileData{content:f.store.put_blob(b"source").unwrap(),mode:0o644,logical:6};
    let base=BTreeMap::new();let mut next=BTreeMap::from([("uncovered.rs".into(),entry.clone())]);assert!(changed_sources_covered(&base,&next,&[policy.profile.clone()]).is_err());
    next.clear();next.insert("lib.rs".into(),entry.clone());next.insert("README.md".into(),entry);changed_sources_covered(&base,&next,&[policy.profile]).unwrap();}
