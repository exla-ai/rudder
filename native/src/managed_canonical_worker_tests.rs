//! The production charge/join helpers with actual threads and TLS. These prove
//! no SDK/helper/process result and never construct a CanonicalGateEvidence.
use super::*;
use std::cell::RefCell;
thread_local!{static TLS:RefCell<Option<TlsHold>>=const{RefCell::new(None)};}
struct TlsHold(std::sync::mpsc::SyncSender<()>,std::sync::mpsc::Receiver<()>);
impl Drop for TlsHold{fn drop(&mut self){self.0.send(()).unwrap();self.1.recv().unwrap();}}
#[test]fn canonical_worker_capacity_remains_charged_until_actual_tls_join(){
    let active=AtomicUsize::new(0);assert!(reserve(&active));let mut charged=true;
    let(entered_tx,entered_rx)=std::sync::mpsc::sync_channel(1);let(release_tx,release_rx)=std::sync::mpsc::sync_channel(1);
    let worker=OwnedWorkerJoin::new(thread::spawn(move||{TLS.with(|slot|*slot.borrow_mut()=Some(TlsHold(entered_tx,release_rx)));}));let mut joined=None;
    assert!(!record_original_join(&worker,&mut joined,&mut charged,&active).unwrap());entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(!record_original_join(&worker,&mut joined,&mut charged,&active).unwrap());assert_eq!(active.load(Ordering::Acquire),1);assert!(charged);
    release_tx.send(()).unwrap();let end=std::time::Instant::now()+Duration::from_secs(5);
    while !record_original_join(&worker,&mut joined,&mut charged,&active).unwrap(){assert!(std::time::Instant::now()<end);thread::yield_now();}
    assert!(joined.is_some());assert!(!charged);assert_eq!(active.load(Ordering::Acquire),0);record_original_join(&worker,&mut joined,&mut charged,&active).unwrap();assert_eq!(active.load(Ordering::Acquire),0);
}
#[test]fn canonical_reservation_shares_the_existing_two_worker_limit(){let active=Arc::new(AtomicUsize::new(0));let workers:Vec<_>=(0..16).map(|_|{let active=active.clone();thread::spawn(move||reserve(&active))}).collect();
    let accepted=workers.into_iter().map(|worker|usize::from(worker.join().unwrap())).sum::<usize>();assert_eq!(accepted,WORKERS);assert_eq!(active.load(Ordering::Acquire),WORKERS);assert!(!reserve(&active));}
