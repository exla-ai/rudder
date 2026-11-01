//! Real Fabric Journal/prepare validation. Does not fabricate a Root config or
//! AcceptedHostCommand and cannot qualify end-to-end owner transfer/transport.
use super::*;
use std::os::unix::fs::DirBuilderExt;
struct Fixture {root:PathBuf,owner:EngineOwnership,selection:AllowedPreparedProject}
impl Fixture {fn new()->Self {
    let root=std::env::temp_dir().join(format!("rudder-ingress-prepare-{}",uuid::Uuid::new_v4()));fs::DirBuilder::new().mode(0o700).create(&root).unwrap();let root=root.canonicalize().unwrap();
    let repo=root.join("repo");let home=root.join("home");for path in [&repo,&home]{fs::DirBuilder::new().mode(0o700).create(path).unwrap();}
    let mut owner=EngineOwnership::acquire_in_home(&repo,NativeAuthority::Fabric,&home).unwrap();let fence=owner.journal_parts().1.clone();let namespace=crate::managed_store::namespace(&home,&repo,&fence.project_id,owner.journal_parts().0).unwrap();
    let prepare=rudder_engine::journal::ManagedProjectPrepare {request_id:uuid::Uuid::new_v4().to_string(),namespace};owner.prepare_managed_project(&prepare).unwrap();
    Self{selection:AllowedPreparedProject{alias:"server-bound-project".into(),checkout:repo,home,cas:root.join("cas"),endpoints:root.join("endpoints"),prepare},root,owner}
}}
impl Drop for Fixture {fn drop(&mut self){let _=self.owner.finish(false);let _=fs::remove_dir_all(&self.root);}}
#[test]
fn prepared_request_must_match_actual_existing_journal_intent_and_floor() {let mut f=Fixture::new();let actual=existing_prepared_project(f.owner.journal_parts().0,&f.selection).unwrap();assert_eq!(actual,f.owner.journal_parts().1.project_id);
    let mut changed=f.selection.clone();changed.prepare.request_id=uuid::Uuid::new_v4().to_string();assert!(existing_prepared_project(f.owner.journal_parts().0,&changed).is_err());}
#[test]
fn copied_prepare_data_cannot_rebind_another_existing_checkout() {let mut f=Fixture::new();let mut changed=f.selection.clone();let other=f.root.join("other");fs::create_dir(&other).unwrap();changed.checkout=other;assert!(existing_prepared_project(f.owner.journal_parts().0,&changed).is_err());}
