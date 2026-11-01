//! Actual thread/TLS and CAS lifetime components. They create no SDK client,
//! no Root ACK/outcome and no source/execution/cache authority.
use super::*;
use rudder_engine::execution::handoff::OwnedWorkerJoin;
use std::{cell::RefCell,collections::BTreeSet,os::unix::fs::DirBuilderExt};
thread_local!{static TLS:RefCell<Option<Tls>>=const{RefCell::new(None)};}
struct Tls(std::sync::mpsc::SyncSender<()>,std::sync::mpsc::Receiver<()>);
impl Drop for Tls{fn drop(&mut self){self.0.send(()).unwrap();self.1.recv().unwrap();}}
#[test]fn sdk_join_binding_rejects_a_different_actual_worker_and_waits_original_tls(){let(entered,at_tls)=std::sync::mpsc::sync_channel(1);let(release,wait)=std::sync::mpsc::sync_channel(1);
    let actual=std::thread::spawn(move||TLS.with(|value|*value.borrow_mut()=Some(Tls(entered,wait))));let expected=actual.thread().id();let actual=OwnedWorkerJoin::new(actual);actual.start_join_observer().unwrap();
    at_tls.recv_timeout(Duration::from_secs(5)).unwrap();assert!(actual.try_joined().unwrap().is_none());let other=OwnedWorkerJoin::new(std::thread::spawn(||{}));let other=other.await_joined(Duration::from_secs(5)).unwrap();
    assert!(require_original_worker(&other,expected).is_err());release.send(()).unwrap();let joined=actual.await_joined(Duration::from_secs(5)).unwrap();require_original_worker(&joined,expected).unwrap();
    let alias=joined.clone();assert!(alias.same_worker(&joined));assert_eq!(alias.original_thread_id_data(),expected);}

struct Directory(PathBuf);impl Directory{fn new()->Self{let base=if Path::new("/workspace").is_dir(){PathBuf::from("/workspace")}else{std::env::temp_dir()};let p=base.join(format!("sdk-job-registry-{}",uuid::Uuid::new_v4()));fs::DirBuilder::new().mode(0o700).create(&p).unwrap();Self(p)}}
impl Drop for Directory{fn drop(&mut self){let _=fs::remove_dir_all(&self.0);}}
struct PinnedResult{store:LocalStore,root:ObjectRef,reader:File}
#[test]fn sdk_slot_bookkeeping_reuses_capacity_without_dropping_real_cas_result_or_reader(){let dir=Directory::new();let store=LocalStore::open(dir.0.join("cas")).unwrap();let payload=store.put_blob(b"actual immutable published artifact").unwrap();
    let root=store.put_directory(rudder_engine::store::DirectoryManifest{format_version:1,mode:0o755,entries:vec![rudder_engine::store::DirectoryEntry::new(b"artifact",rudder_engine::store::EntryNode::File{object:payload.clone(),mode:0o644}).unwrap()]}).unwrap();store.pin("actual-artifact",&root).unwrap();
    let actual=Arc::new(PinnedResult{store:store.clone(),root:root.clone(),reader:File::open(store.object_path(&payload).unwrap()).unwrap()});let weak=Arc::downgrade(&actual);let mut registry=vec![actual.clone()];
    let foreign=Arc::new(PinnedResult{store:store.clone(),root:root.clone(),reader:File::open(store.object_path(&payload).unwrap()).unwrap()});assert!(remove_original_slot(&mut registry,&foreign).is_err());assert_eq!(registry.len(),1);
    remove_original_slot(&mut registry,&actual).unwrap();assert!(registry.is_empty());registry.push(foreign);assert_eq!(registry.len(),1);assert!(weak.upgrade().is_some());
    let garbage=store.put_blob(b"unretained garbage").unwrap();store.gc(Duration::ZERO).unwrap();assert!(!store.object_path(&garbage).unwrap().exists());assert_eq!(store.retention().unwrap().pins.get("actual-artifact"),Some(&root));
    let refs:BTreeSet<_>=actual.store.retained_closure(&actual.root).unwrap().into_iter().collect();assert!(refs.contains(&payload));let mut bytes=vec![0;payload.size_bytes as usize];assert_eq!(actual.reader.read_at(&mut bytes,0).unwrap(),bytes.len());assert_eq!(bytes,b"actual immutable published artifact");}
#[test]fn sdk_cleanup_lock_uses_the_original_cancelled_clock_without_replacing_it(){let lock=Mutex::new(7u32);let original=FinalizationBudget::begin(Duration::from_secs(5)).unwrap();let held=lock.lock().unwrap();original.cancel();
    assert!(super::lock(&lock,&original,&AtomicBool::new(false)).is_err());drop(held);assert!(super::lock(&lock,&original,&AtomicBool::new(false)).is_err());}
