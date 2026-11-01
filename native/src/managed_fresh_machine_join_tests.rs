use super::*;
use std::{cell::RefCell,sync::mpsc};
thread_local!{static HELD:RefCell<Option<WaitInTls>>=const{RefCell::new(None)};}
struct WaitInTls{entered:mpsc::SyncSender<()>,release:mpsc::Receiver<()>}
impl Drop for WaitInTls{fn drop(&mut self){let _=self.entered.send(());let _=self.release.recv();}}
#[test]
fn original_flow_body_return_is_not_tls_join_and_bare_join_cannot_mint_assigned_authority(){
    let flow=FlowCell::capture("no-authority-fixture".into(),Arc::new(AtomicBool::new(false)));flow.original.set(FinalizationBudget::begin(Duration::from_secs(5)).unwrap()).ok().unwrap();let (at_tls,tls)=mpsc::sync_channel(1);let (release,wait)=mpsc::sync_channel(1);let (body,returned)=mpsc::sync_channel(1);let actual=flow.clone();
    let worker=thread::spawn(move||{while !actual.owned.load(Ordering::Acquire){thread::yield_now();}HELD.with(|slot|*slot.borrow_mut()=Some(WaitInTls{entered:at_tls,release:wait}));body.send(()).unwrap();});
    *flow.worker.lock().unwrap()=Some(OwnedWorkerJoin::new(worker));flow.owned.store(true,Ordering::Release);returned.recv_timeout(Duration::from_secs(2)).unwrap();tls.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(!flow.poll_join_retaining().unwrap());assert!(flow.qualified.get().is_none());assert!(flow.late_join.lock().unwrap().is_none());assert!(flow.worker.lock().unwrap().is_some());
    release.send(()).unwrap();loop{if flow.poll_join_retaining().unwrap(){panic!("no original assigned source exists");}if flow.late_join.lock().unwrap().is_some(){break;}flow.original.get().unwrap().remaining().unwrap();thread::yield_now();}
    assert!(flow.qualified.get().is_none());assert!(flow.denied.load(Ordering::Acquire));assert!(flow.worker.lock().unwrap().is_some());
    // No Root ACK, image, namespace, lease or ready/assigned capsule is forged.
}
