//! Real original SQLite-owned keeper and checkout-lock aliases; no Root/PG
//! owner is fabricated. These source definitions are UNRUN by the author.
use super::*;
#[test]
fn actual_source_lease_clone_shares_original_keeper_and_retains_original_lock(){
    let scratch=Scratch::new();let(repo,_home,mut owner)=source_owner_fixture(&scratch,NativeAuthority::Fabric);
    let original=owner.source_lease(&owner.fence).unwrap();let alias=original.clone();
    assert!(Arc::ptr_eq(&original.database,&alias.database));assert!(Arc::ptr_eq(&original.lifetime,&alias.lifetime));
    assert_eq!(original.journal_identity,alias.journal_identity);drop(original);alias.check_current().unwrap();
    owner.finish(false).unwrap();assert!(alias.check_current().is_err());drop(owner);
    probe_source_owner_lock_in_fresh_process(&repo,true);drop(alias);probe_source_owner_lock_in_fresh_process(&repo,false);
}
#[test]
fn original_database_replacement_fences_every_alias_without_reopen(){
    let scratch=Scratch::new();let(_repo,_home,mut owner)=source_owner_fixture(&scratch,NativeAuthority::Fabric);
    let original=owner.source_lease(&owner.fence).unwrap();let alias=original.clone();let path=owner.journal.database_path().unwrap().to_path_buf();let moved=path.with_extension("retained.sqlite");
    let bytes=fs::metadata(&path).unwrap().len();fs::rename(&path,&moved).unwrap();File::create(&path).unwrap().set_len(bytes).unwrap();
    assert!(original.check_current().is_err());fs::remove_file(&path).unwrap();fs::rename(&moved,&path).unwrap();
    assert!(alias.check_current().is_err());assert!(original.check_current().is_err());let _=owner.finish(false);
}
