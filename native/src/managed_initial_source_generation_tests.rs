//! Actual held local Engine/Journal lifetime negatives, not synthetic PG grants.
use super::*;
#[test]
fn owner_retirement_after_source_ack_keeps_old_seed_pins_but_refuses_new_baselines(){
    let mut f=Fixture::new();let original=budget();let cancel=AtomicBool::new(false);
    f.source.prepare(&f.environment,&f.settings,&original,&cancel).unwrap();
    let prepared=f.source.prepared.as_ref().unwrap();let seed=prepared.seed_root.clone();let pin=prepared.seed_pin.clone();
    f.engine.finish(false).unwrap();
    assert!(f.source.prepared_current(&budget(),&cancel).is_err());
    assert!(f.source.check_shared_admission().is_err());assert!(f.source.prepared.is_some());
    assert_eq!(f.source.store.retention().unwrap().pins.get(&pin),Some(&seed));
}
#[test]
fn cancelled_snapshot_validation_keeps_original_acked_bytes_without_replaying_generation(){
    let mut f=Fixture::new();let original=budget();let cancel=AtomicBool::new(false);
    f.source.prepare(&f.environment,&f.settings,&original,&cancel).unwrap();
    let prepared=f.source.prepared.as_ref().unwrap();let root=prepared.seed_root.clone();let pin=prepared.seed_pin.clone();
    original.cancel();assert!(f.source.prepared_current(&original,&cancel).is_err());
    assert!(f.source.prepare(&f.environment,&f.settings,&budget(),&cancel).is_err());
    assert!(f.source.acknowledged&&f.source.prepared.is_some());assert_eq!(f.source.store.retention().unwrap().pins.get(&pin),Some(&root));
}

#[test]
fn source_pin_loss_is_detected_without_using_saved_refs_as_replacement_retention(){
    let mut f=Fixture::new();let original=budget();let cancel=AtomicBool::new(false);
    f.source.prepare(&f.environment,&f.settings,&original,&cancel).unwrap();
    let prepared=f.source.prepared.as_ref().unwrap();let seed=prepared.seed_root.clone();let pin=prepared.seed_pin.clone();
    f.source.store.unpin(&pin).unwrap();assert!(f.source.prepared_current(&budget(),&cancel).is_err());
    assert!(f.source.prepared.is_some()&&f.source.acknowledged);assert_eq!(f.source.prepared.as_ref().unwrap().seed_root,seed);
    assert!(!f.source.store.retention().unwrap().pins.contains_key(&pin));
}
