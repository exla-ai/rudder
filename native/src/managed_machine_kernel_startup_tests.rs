//! Actual descriptor and worker negatives only. These fixtures cannot create
//! RegisteredEngineOwner, KernelStartupOwner, a Root ACK, or PG authority.
use super::*;
use std::{cell::RefCell,os::unix::fs::{DirBuilderExt,PermissionsExt},sync::mpsc::{self,Receiver,SyncSender},time::Instant};
struct PrivateDirectory {path:PathBuf}
impl PrivateDirectory {fn new()->Self{let path=std::env::temp_dir().join(format!("rudder-kernel-caller-{}",uuid::Uuid::new_v4()));fs::DirBuilder::new().mode(0o700).create(&path).unwrap();Self{path}}}
impl Drop for PrivateDirectory{fn drop(&mut self){let _=fs::remove_dir_all(&self.path);}}
#[test]
fn actual_held_state_directory_refuses_replacement_and_preserves_original_descriptor(){
    let f=PrivateDirectory::new();let held=Directory::capture(&f.path,unsafe{libc::geteuid()},unsafe{libc::getegid()}).unwrap();let renamed=f.path.with_extension("original");
    fs::rename(&f.path,&renamed).unwrap();fs::DirBuilder::new().mode(0o700).create(&f.path).unwrap();
    assert!(held.current().is_err());assert!(held.file.metadata().unwrap().is_dir());fs::remove_dir_all(renamed).unwrap();
}
#[test]
fn state_directory_permissions_are_not_adopted_or_repaired_by_startup_capture(){
    let f=PrivateDirectory::new();fs::set_permissions(&f.path,fs::Permissions::from_mode(0o755)).unwrap();
    assert!(Directory::capture(&f.path,unsafe{libc::geteuid()},unsafe{libc::getegid()}).is_err());assert_eq!(fs::metadata(&f.path).unwrap().mode()&0o777,0o755);
}
struct HeldTls {entered:SyncSender<()>,release:Receiver<()>}
impl Drop for HeldTls{fn drop(&mut self){let _=self.entered.send(());let _=self.release.recv_timeout(Duration::from_secs(10));}}
thread_local!{static HELD:RefCell<Option<HeldTls>>=const{RefCell::new(None)};}
fn actual_worker()->(Worker,Receiver<()>,SyncSender<()>){let(entered,receive)=mpsc::sync_channel(1);let(release,wait)=mpsc::sync_channel(1);
    let thread=std::thread::spawn(move||{HELD.with(|held|*held.borrow_mut()=Some(HeldTls{entered,release:wait}));});let mut owner=Worker::new();owner.attempted=true;owner.original=Some(OwnedWorkerJoin::new(thread));(owner,receive,release)}
fn actual_join(worker:&Worker)->WorkerJoined{let end=Instant::now()+Duration::from_secs(10);loop{if let Some(proof)=worker.poll_joined().unwrap(){return proof;}assert!(Instant::now()<end);std::thread::yield_now();}}
#[test]
fn controller_body_return_cannot_replace_actual_tls_join_or_release_original_handle(){
    let (worker,entered,release)=actual_worker();entered.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(worker.poll_joined().unwrap().is_none());assert!(worker.original.is_some());release.send(()).unwrap();let proof=actual_join(&worker);
    assert!(proof.same_worker(&worker.poll_joined().unwrap().unwrap()));
}
#[test]
fn a_late_real_join_does_not_reset_the_original_startup_deadline(){
    let (worker,entered,release)=actual_worker();entered.recv_timeout(Duration::from_secs(5)).unwrap();let original=FinalizationBudget::begin(Duration::from_secs(10)).unwrap();let same=original.clone();original.cancel();
    release.send(()).unwrap();let proof=actual_join(&worker);assert!(time(&same,&AtomicBool::new(false)).is_err());assert!(proof.same_worker(&worker.poll_joined().unwrap().unwrap()));assert!(worker.original.is_some());
}
#[test]
fn an_unconfigured_or_closed_slot_cannot_mint_a_startup_ticket_from_status(){
    let slot=KernelSlot::new();assert_eq!(slot.status(),KernelStartupStatus::Pending);assert!(slot.ticket().is_err());
    slot.close();slot.phase.store(ADMITTED,Ordering::Release);assert_eq!(slot.status(),KernelStartupStatus::Closed);assert!(slot.ticket().is_err());assert!(slot.capability.get().is_none());
}
#[test]
fn retirement_refuses_unresolved_probe_construction_without_consuming_a_cleanup_clock(){
    let slot=KernelSlot::new();let original=FinalizationBudget::begin(Duration::from_secs(10)).unwrap();assert!(slot.retire_retaining(original).is_err());
    assert!(slot.retirement_clock.get().is_none());let held=slot.owners.lock().unwrap();assert!(held.unused.is_some()&&held.attempted.is_none()&&held.probe.is_none()&&held.owner.is_none());assert_eq!(slot.status(),KernelStartupStatus::Closed);
}
