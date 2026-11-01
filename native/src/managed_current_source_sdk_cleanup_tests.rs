//! Actual threads/TLS only; no synthetic SDK/client/Root ACK is created.
use super::*;
use rudder_engine::execution::handoff::OwnedWorkerJoin;
use std::{cell::RefCell,sync::mpsc};
struct HeldTls(mpsc::Receiver<()>);
impl Drop for HeldTls{fn drop(&mut self){self.0.recv_timeout(Duration::from_secs(2)).unwrap();}}
thread_local!{static TLS:RefCell<Option<HeldTls>>=const{RefCell::new(None)};}
#[test]fn body_completion_is_not_actual_tls_join_and_foreign_join_never_matches_original(){let(body_send,body_receive)=mpsc::sync_channel(1);let(release_send,release_receive)=mpsc::sync_channel(1);let(start_send,start_receive)=mpsc::sync_channel(1);
 let worker=std::thread::spawn(move||{start_receive.recv().unwrap();TLS.with(|slot|*slot.borrow_mut()=Some(HeldTls(release_receive)));body_send.send(()).unwrap();});let id=worker.thread().id();let owned=OwnedWorkerJoin::new(worker);start_send.send(()).unwrap();owned.start_join_observer().unwrap();body_receive.recv_timeout(Duration::from_secs(2)).unwrap();assert!(owned.try_joined().unwrap().is_none());release_send.send(()).unwrap();let joined=owned.await_joined(Duration::from_secs(2)).unwrap();require_join(&joined,id).unwrap();
 let foreign=std::thread::spawn(||{});let other=OwnedWorkerJoin::new(foreign);other.start_join_observer().unwrap();let other=other.await_joined(Duration::from_secs(2)).unwrap();assert!(require_join(&other,id).is_err());}

#[test]fn routine_error_still_requires_both_actual_original_worker_and_cleanup_tls_joins(){
 let(error_send,error_receive)=mpsc::sync_channel(1);let(release_build,held_build)=mpsc::sync_channel(1);
 let worker=std::thread::spawn(move||{TLS.with(|slot|*slot.borrow_mut()=Some(HeldTls(held_build)));error_send.send(Err::<(),_>("actual worker routine error DATA")).unwrap();});
 let build_id=worker.thread().id();let build=OwnedWorkerJoin::new(worker);build.start_join_observer().unwrap();
 assert!(error_receive.recv_timeout(Duration::from_secs(2)).unwrap().is_err());assert!(build.try_joined().unwrap().is_none());
 release_build.send(()).unwrap();let build_joined=build.await_joined(Duration::from_secs(2)).unwrap();require_join(&build_joined,build_id).unwrap();
 let(return_send,return_receive)=mpsc::sync_channel(1);let(release_cleanup,held_cleanup)=mpsc::sync_channel(1);let original_build=build_joined.clone();
 let cleanup=std::thread::spawn(move||{require_join(&original_build,build_id).unwrap();TLS.with(|slot|*slot.borrow_mut()=Some(HeldTls(held_cleanup)));return_send.send(()).unwrap();});
 let cleanup_id=cleanup.thread().id();assert_ne!(cleanup_id,build_id);let cleanup=OwnedWorkerJoin::new(cleanup);cleanup.start_join_observer().unwrap();
 return_receive.recv_timeout(Duration::from_secs(2)).unwrap();assert!(cleanup.try_joined().unwrap().is_none());assert!(require_join(&build_joined,cleanup_id).is_err());
 release_cleanup.send(()).unwrap();let actual_cleanup=cleanup.await_joined(Duration::from_secs(2)).unwrap();require_join(&actual_cleanup,cleanup_id).unwrap();assert!(require_join(&actual_cleanup,build_id).is_err());
 // This tests genuine thread/TLS evidence only. An Err is never an SDK receipt;
 // no SDK client, terminal-failure fact, artifact or settled token is forged.
}
