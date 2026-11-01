//! Real worker/TLS ownership tests. No Source/Root/PG owner is constructed.
use super::*;
use std::{cell::RefCell,sync::mpsc::{self,Receiver,SyncSender},time::Instant};
struct HeldTls {entered:SyncSender<()>,release:Receiver<()>}
impl Drop for HeldTls {fn drop(&mut self){let _=self.entered.send(());let _=self.release.recv_timeout(Duration::from_secs(3));}}
thread_local! {static HELD:RefCell<Option<HeldTls>>=const{RefCell::new(None)};}
fn tls_worker()->(Worker,Receiver<()>,SyncSender<()>) {
    let (entered,receive)=mpsc::sync_channel(1);let (release,wait)=mpsc::sync_channel(1);
    let handle=std::thread::spawn(move||{HELD.with(|slot|*slot.borrow_mut()=Some(HeldTls{entered,release:wait}));});
    let mut worker=Worker::new();worker.attempted=true;worker.handle=Some(OwnedWorkerJoin::new(handle));
    (worker,receive,release)
}
fn joined(worker:&Worker)->WorkerJoined {
    let end=Instant::now()+Duration::from_secs(3);
    loop {if let Some(joined)=worker.joined().unwrap(){return joined;}assert!(Instant::now()<end,"actual original worker did not join");std::thread::yield_now();}
}
#[test]
fn returned_factory_body_keeps_original_thread_and_tls_owned_until_actual_join(){
    let (worker,entered,release)=tls_worker();entered.recv_timeout(Duration::from_secs(2)).unwrap();
    let original=FinalizationBudget::begin(Duration::from_secs(3)).unwrap();let cancel=AtomicBool::new(false);
    assert!(qualified_factory(&worker,&original,&cancel).unwrap().is_none());assert!(worker.handle.is_some());
    release.send(()).unwrap();let proof=joined(&worker);
    let qualified=qualified_factory(&worker,&original,&cancel).unwrap().unwrap();assert!(proof.same_worker(&qualified));
}
#[test]
fn original_snapshot_cutoff_cannot_be_reset_by_a_late_genuine_factory_join(){
    let (worker,entered,release)=tls_worker();entered.recv_timeout(Duration::from_secs(2)).unwrap();
    let original=FinalizationBudget::begin(Duration::from_secs(3)).unwrap();original.cancel();let cancel=AtomicBool::new(false);
    release.send(()).unwrap();let actual_join=joined(&worker);
    assert!(qualified_factory(&worker,&original,&cancel).is_err());
    assert!(actual_join.same_worker(&worker.joined().unwrap().unwrap()));assert!(worker.handle.is_some());
}
#[test]
fn panicked_original_factory_retains_actual_join_error_and_cannot_qualify_snapshot(){
    let mut worker=Worker::new();worker.attempted=true;worker.handle=Some(OwnedWorkerJoin::new(std::thread::spawn(||panic!("actual factory panic"))));
    let end=Instant::now()+Duration::from_secs(3);loop{match worker.joined(){Err(_)=>break,Ok(None)=>{},Ok(Some(_))=>panic!("panic became a successful join")};assert!(Instant::now()<end);std::thread::yield_now();}
    let original=FinalizationBudget::begin(Duration::from_secs(3)).unwrap();assert!(qualified_factory(&worker,&original,&AtomicBool::new(false)).is_err());assert!(worker.handle.is_some());
}
