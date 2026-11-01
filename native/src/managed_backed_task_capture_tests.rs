//! Real local Engine/Journal/jj boundary cases. They cannot construct a Root,
//! PG/runtime owner and therefore must never promote local/path DATA to one.
use super::*;
#[test]
fn actual_legacy_source_remains_local_in_the_new_task_capture_api(){
    let mut f=Fixture::new();let original=budget();let cancel=Arc::new(AtomicBool::new(false));
    f.source.prepare(&f.environment,&f.settings,&original,&cancel).unwrap();let mut out=None;
    f.source.capture_task_backing_retaining(&mut out,&original,&cancel).unwrap();assert!(out.is_none());
    let prepared=f.source.prepared_current(&original,&cancel).unwrap();assert!(prepared.runnable_inputs().is_none());
    assert_eq!(f.source.store.retention().unwrap().pins.get(&prepared.runnable_pin),Some(&prepared.runnable_root));
}
#[test]
fn source_result_without_original_ack_cannot_issue_backed_task_inputs(){
    let mut f=Fixture::new();let original=budget();let cancel=Arc::new(AtomicBool::new(false));
    assert!(f.source.prepare_inner(&f.environment,&f.settings,&original,&cancel,||Err(anyhow::anyhow!("lost original source ACK"))).is_err());
    assert!(f.source.prepared.is_some());let mut out=None;
    assert!(f.source.capture_task_backing_retaining(&mut out,&budget(),&cancel).is_err());assert!(out.is_none());
    assert!(!f.source.acknowledged);
}
#[test]
fn actual_retired_owner_cannot_turn_a_retained_local_root_into_task_backing(){
    let mut f=Fixture::new();let original=budget();let cancel=Arc::new(AtomicBool::new(false));
    f.source.prepare(&f.environment,&f.settings,&original,&cancel).unwrap();let root=f.source.prepared.as_ref().unwrap().runnable_root.clone();
    f.engine.finish(false).unwrap();let mut out=None;
    assert!(f.source.capture_task_backing_retaining(&mut out,&budget(),&cancel).is_err());assert!(out.is_none());
    assert!(f.source.store.retention().unwrap().pins.values().any(|r|r==&root));
}
