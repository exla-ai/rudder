use super::*;
struct Clock(FinalizationBudget);
impl ImageImportFence for Clock{fn remaining(&self)->Result<Duration>{self.0.remaining()}}
fn budget()->FinalizationBudget{FinalizationBudget::begin(Duration::from_secs(30)).unwrap()}
fn fixture()->(tempfile::TempDir,LocalStore){let temp=if Path::new("/workspace").is_dir(){tempfile::tempdir_in("/workspace").unwrap()}else{tempfile::tempdir().unwrap()};
    fs::set_permissions(temp.path(),fs::Permissions::from_mode(0o700)).unwrap();
    let store=LocalStore::open(temp.path().join("cas")).unwrap();(temp,store)}
#[test]fn shared_index_original_keeper_is_acquired_once_without_original_clock_callback_leak(){
    let(temp,store)=fixture();let original=budget();let mut out=None;BuildIndex::create_shared_retaining(&mut out,temp.path().canonicalize().unwrap().join("index"),store,original.clone(),Arc::new(AtomicBool::new(false)),Arc::new(Clock(original.clone()))).unwrap();
    let index=out.as_ref().unwrap().index().unwrap();original.cancel();let keeper=index.connection.lock().unwrap();
    assert_eq!(keeper.query_row("WITH RECURSIVE q(x) AS (VALUES(0) UNION ALL SELECT x+1 FROM q WHERE x<5000) SELECT sum(x) FROM q",[],|r|r.get::<_,i64>(0)).unwrap(),12502500);
}
#[test]fn shared_index_refuses_existing_child_without_opening_or_adopting_database(){
    let(temp,store)=fixture();let path=temp.path().canonicalize().unwrap().join("index");fs::create_dir(&path).unwrap();let original=budget();let mut out=None;
    assert!(BuildIndex::create_shared_retaining(&mut out,path.clone(),store,original.clone(),Arc::new(AtomicBool::new(false)),Arc::new(Clock(original))).is_err());
    let held=out.as_ref().unwrap();assert!(held.mkdir_attempted&&!held.sqlite_attempted&&held.connection.is_none());assert!(held.index().is_err());assert!(!path.join("index.sqlite").exists());
}
#[test]fn shared_index_cancelled_original_stages_owner_before_any_directory_effect(){
    let(temp,store)=fixture();let path=temp.path().canonicalize().unwrap().join("index");let original=budget();original.cancel();let mut out=None;
    assert!(BuildIndex::create_shared_retaining(&mut out,path.clone(),store,original.clone(),Arc::new(AtomicBool::new(false)),Arc::new(Clock(original))).is_err());
    assert!(out.is_some());assert!(!out.as_ref().unwrap().mkdir_attempted);assert!(out.as_ref().unwrap().index().is_err());assert!(!path.exists());
}
#[test]fn shared_index_original_sqlite_replacement_refuses_and_retains_keeper(){
    let(temp,store)=fixture();let path=temp.path().canonicalize().unwrap().join("index");let original=budget();let mut out=None;
    BuildIndex::create_shared_retaining(&mut out,path.clone(),store.clone(),original.clone(),Arc::new(AtomicBool::new(false)),Arc::new(Clock(original))).unwrap();
    let replacement=temp.path().join("replacement");drop(BuildIndex::open(&replacement,store).unwrap());let displaced=temp.path().join("displaced");
    fs::rename(&path,&displaced).unwrap();fs::rename(&replacement,&path).unwrap();let failed=out.as_ref().unwrap().check().is_err();
    fs::rename(&path,&replacement).unwrap();fs::rename(&displaced,&path).unwrap();assert!(failed);assert!(out.as_ref().unwrap().index.is_some());
}
