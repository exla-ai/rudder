//! Real kernel lock/FD tests. Deliberately incomplete jj repository never
//! constructs an acceptor, task/Root authority or compiler gate evidence.
use super::*;
use std::os::unix::fs::DirBuilderExt;
struct Directory(PathBuf);
impl Directory{fn new()->Self{let path=std::env::temp_dir().join(format!("canonical-acquire-{}",uuid::Uuid::new_v4()));fs::DirBuilder::new().mode(0o700).create(&path).unwrap();Self(path.canonicalize().unwrap())}}
impl Drop for Directory{fn drop(&mut self){let _=fs::remove_dir_all(&self.0);}}
fn budget()->FinalizationBudget{FinalizationBudget::begin(Duration::from_secs(30)).unwrap()}
fn release()->UnpublishedRelease{UnpublishedRelease{lock:None,path:PathBuf::new(),attempted:false,ack:false,timely:false}}
fn competing(path:&std::path::Path)->File{OpenOptions::new().read(true).write(true).open(path.join("rudder-acceptance.lock")).unwrap()}
#[test]fn canonical_partial_jj_open_error_retains_actual_acquired_writer_until_explicit_unlock(){let dir=Directory::new();let mut out=None;let mut ready=None;
    assert!(RetainedIntegrationOpen::open_retaining(&mut out,&mut ready,dir.0.clone(),"component","component@example.invalid",&budget(),&AtomicBool::new(false)).is_err());
    assert!(ready.is_none());let actual=out.as_ref().unwrap();assert!(matches!(actual.acquisition,Acquisition::Owned));let other=competing(&dir.0);assert_eq!(other.try_lock_exclusive().unwrap_err().kind(),std::io::ErrorKind::WouldBlock);
    let mut ack=release();unlock(actual.lock.as_ref().unwrap(),&actual.path,&mut ack,&budget(),&AtomicBool::new(false)).unwrap();assert!(ack.ack);assert!(!ack.acknowledged());
    // Physical unlock alone is not the enclosing timely canonical cleanup ACK.
    other.try_lock_exclusive().unwrap();FileExt::unlock(&other).unwrap();
}
#[test]fn canonical_known_busy_acquisition_keeps_original_competing_writer_untouched(){let dir=Directory::new();let original=OpenOptions::new().read(true).write(true).create_new(true).mode(0o600).open(dir.0.join("rudder-acceptance.lock")).unwrap();original.try_lock_exclusive().unwrap();
    let mut out=None;let mut ready=None;assert!(RetainedIntegrationOpen::open_retaining(&mut out,&mut ready,dir.0.clone(),"component","component@example.invalid",&budget(),&AtomicBool::new(false)).is_err());
    assert!(matches!(out.as_ref().unwrap().acquisition,Acquisition::Busy));assert!(ready.is_none());assert_eq!(competing(&dir.0).try_lock_exclusive().unwrap_err().kind(),std::io::ErrorKind::WouldBlock);FileExt::unlock(&original).unwrap();}
#[test]fn canonical_replaced_lock_refuses_unlock_and_retains_the_actual_original_fd(){let dir=Directory::new();let mut out=None;let mut ready=None;let _=RetainedIntegrationOpen::open_retaining(&mut out,&mut ready,dir.0.clone(),"component","component@example.invalid",&budget(),&AtomicBool::new(false));
    let actual=out.as_ref().unwrap();let original_path=dir.0.join("original.lock");fs::rename(dir.0.join("rudder-acceptance.lock"),&original_path).unwrap();
    OpenOptions::new().write(true).create_new(true).mode(0o600).open(dir.0.join("rudder-acceptance.lock")).unwrap();let mut ack=release();
    assert!(unlock(actual.lock.as_ref().unwrap(),&actual.path,&mut ack,&budget(),&AtomicBool::new(false)).is_err());assert!(!ack.acknowledged());
    let original=OpenOptions::new().read(true).write(true).open(&original_path).unwrap();assert_eq!(original.try_lock_exclusive().unwrap_err().kind(),std::io::ErrorKind::WouldBlock);
}
#[test]fn canonical_cancel_before_acquire_preserves_unconsumed_original_acquisition_permit(){let dir=Directory::new();let clock=budget();clock.cancel();let mut out=None;let mut ready=None;
    assert!(RetainedIntegrationOpen::open_retaining(&mut out,&mut ready,dir.0.clone(),"component","component@example.invalid",&clock,&AtomicBool::new(false)).is_err());
    assert!(matches!(out.as_ref().unwrap().acquisition,Acquisition::Unattempted(_)));assert!(ready.is_none());assert!(!dir.0.join("rudder-acceptance.lock").exists());}
#[test]fn canonical_retaining_open_keeps_original_real_jj_loader_and_lock(){let dir=Directory::new();
    let(_workspace,_repo)=pollster::block_on(jj_lib::workspace::Workspace::init_internal_git(&settings("Component","component@example.invalid").unwrap(),&dir.0)).unwrap();
    let mut out=None;let mut ready=None;RetainedIntegrationOpen::open_retaining(&mut out,&mut ready,dir.0.clone(),"Component","component@example.invalid",&budget(),&AtomicBool::new(false)).unwrap();
    let actual=ready.as_ref().unwrap();actual.check_current().unwrap();assert_eq!(competing(&dir.0).try_lock_exclusive().unwrap_err().kind(),std::io::ErrorKind::WouldBlock);
    let heads=pollster::block_on(actual.loader.op_heads_store().get_op_heads()).unwrap();assert_eq!(heads.len(),1);let operation=pollster::block_on(actual.loader.load_operation(&heads[0])).unwrap();
    let repo=pollster::block_on(actual.loader.load_at(&operation)).unwrap();assert!(repo.view().get_wc_commit_id(jj_lib::ref_name::WorkspaceName::DEFAULT).is_some());}
