//! Restriction-only test hook: an ACTUAL mutex held by another thread may
//! deny a genuinely acquired local owner before physical IO. It never supplies
//! Root/PG custody or success and is absent from production builds.
use super::*;
use std::{cell::RefCell,sync::{Mutex,TryLockError,mpsc},thread};
thread_local!{static PROBE:RefCell<Option<Arc<Mutex<()>>>>=const{RefCell::new(None)};}
pub(in crate::engine_host) fn restriction()->Result<()>{PROBE.with(|slot|{let held=slot.borrow();if let Some(probe)=&*held{match probe.try_lock(){
    Ok(_)=>{},Err(TryLockError::WouldBlock)=>return Err(EngineSharedAdmissionBusy.into()),Err(_)=>anyhow::bail!("actual test custody restriction poisoned")}}Ok(())})}
struct Probe;
impl Probe{fn set(probe:Arc<Mutex<()>>)->Self{PROBE.with(|p|*p.borrow_mut()=Some(probe));Self}}
impl Drop for Probe{fn drop(&mut self){PROBE.with(|p|p.borrow_mut().take());}}
fn hold(probe:Arc<Mutex<()>>)->(mpsc::Sender<()>,thread::JoinHandle<()>){let(started,ready)=mpsc::channel();let(release,wait)=mpsc::channel();
    let worker=thread::spawn(move||{let _guard=probe.lock().unwrap();started.send(()).unwrap();wait.recv().unwrap();});ready.recv_timeout(Duration::from_secs(5)).unwrap();(release,worker)}
#[test]
fn actual_typed_contention_then_release_preserves_real_engine_lease(){
    let scratch=Scratch::new();let(_repo,_home,mut owner)=source_owner_fixture(&scratch,NativeAuthority::Fabric);let fence=owner.fence.clone();let lease=owner.source_lease(&fence).unwrap();
    let probe=Arc::new(Mutex::new(()));let _scope=Probe::set(probe.clone());let(release,worker)=hold(probe);
    let error=lease.check_current().unwrap_err();assert!(error.downcast_ref::<EngineSharedAdmissionBusy>().is_some());assert!(!lease.lifetime.revoked.load(Ordering::Acquire));
    release.send(()).unwrap();worker.join().unwrap();lease.check_current().unwrap();owner.assert_current().unwrap();
    owner.finish(false).unwrap();assert!(lease.check_current().is_err());
}
#[test]
fn physical_replacement_after_contention_is_absorbing_even_if_contention_returns(){
    let scratch=Scratch::new();let(repo,_home,mut owner)=source_owner_fixture(&scratch,NativeAuthority::Fabric);let lease=owner.source_lease(&owner.fence).unwrap();
    let probe=Arc::new(Mutex::new(()));let _scope=Probe::set(probe.clone());let(release,worker)=hold(probe.clone());assert!(custody_busy(&lease.check_current().unwrap_err()));release.send(()).unwrap();worker.join().unwrap();
    let named=repo.join(".rudder/engine/owner.lock");let retained=repo.join(".rudder/engine/retained.lock");fs::rename(&named,&retained).unwrap();File::create(&named).unwrap();
    let physical=lease.check_current().unwrap_err();assert!(!custody_busy(&physical));assert!(lease.lifetime.revoked.load(Ordering::Acquire));
    fs::remove_file(&named).unwrap();fs::rename(&retained,&named).unwrap();let(release,worker)=hold(probe);assert!(!custody_busy(&lease.check_current().unwrap_err()));release.send(()).unwrap();worker.join().unwrap();
    assert!(lease.check_current().is_err());assert!(owner.source_lease(&owner.fence.clone()).is_err());let _=owner.finish(false);
}
