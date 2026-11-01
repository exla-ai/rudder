use super::*;
use std::os::unix::fs::DirBuilderExt;
#[test]
fn raw_uuid_and_even_real_local_journal_floor_do_not_admit_fresh_namespace_through_legacy_open(){
    let root=std::env::temp_dir().join(format!("rudder-fresh-store-boundary-{}",uuid::Uuid::new_v4()));fs::DirBuilder::new().mode(0o700).create(&root).unwrap();let home=root.join("home");fs::DirBuilder::new().mode(0o700).create(&home).unwrap();fs::DirBuilder::new().mode(0o700).create(home.join("fabric")).unwrap();let repo=root.join("repository");fs::DirBuilder::new().mode(0o700).create(&repo).unwrap();
    let mut journal=Journal::open(home.join("fabric/journal.sqlite")).unwrap();let id=uuid::Uuid::new_v4().to_string();let owner=journal.create_project(&id,"actual-local-test-only","1111111111111111111111111111111111111111",1).unwrap();
    let request=ManagedProjectPrepare{request_id:uuid::Uuid::new_v4().to_string(),namespace:ManagedProjectNamespace{format_version:1,project_id:id.clone(),home:home.clone(),journal:home.join("fabric/journal.sqlite"),repository:repo.clone(),directory:ManagedProjectNamespace::directory_for(&home,&id)}};
    journal.prepare_managed_project(&owner,&request,2).unwrap();assert!(namespace(&home,&repo,&id,&journal).is_err());assert!(persist_prepare_intent(&request,&journal).is_err());assert!(ManagedStore::open(&journal,&request.namespace).is_err());assert!(!request.namespace.directory.exists());assert_eq!(journal.project_state(&id).unwrap().owner,owner);assert!(journal.managed_project(&id).unwrap().is_some());drop(journal);fs::remove_dir_all(root).unwrap();
    // Actual local Journal DATA is deliberately NOT a Root/PG preparation,
    // FreshNamespaceCustody, EngineSourceLease or remotely assigned owner.
}
