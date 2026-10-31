use super::*;
use crate::protocol::canonical_json;

fn fixture() -> (tempfile::TempDir, TransferSession) {
    let root = tempfile::tempdir().unwrap();
    let session = TransferSession::open(&FabricPaths::new(root.path())).unwrap();
    (root, session)
}

fn pending(session: &mut TransferSession, stage: &str, pin: &str, refs: &[ObjectRef]) {
    session.partial_pins.insert(
        pin.into(),
        PartialTransferPin {
            staging_id: stage.into(),
            expected: refs.into(),
            receipt: None,
            inventory: BTreeSet::new(),
        },
    );
}

#[test]
fn transfer_staging_fences_real_gc_after_validation_and_before_clone_ack() {
    let (_root, mut session) = fixture();
    let producer = session
        .store
        .begin_staging(Duration::from_secs(60))
        .unwrap();
    let mut refs = [b"capture".as_slice(), b"evidence", b"wrapper"]
        .iter()
        .map(|bytes| session.store.put_blob(bytes).unwrap())
        .collect::<Vec<_>>();
    refs.sort();
    let producer = session
        .store
        .retain_object_set(&producer.id, "producer", &refs)
        .unwrap();
    let stage = session
        .store
        .begin_staging(Duration::from_secs(60))
        .unwrap();
    session.staging.insert(stage.id.clone());
    assert_eq!(session.store.object_set_refs(&producer).unwrap(), refs);
    // Validation finished; the producer can now retire. The transport still
    // has no readable inventory until its own actual durable ACK succeeds.
    session.store.release_object_set(&producer).unwrap();
    for reference in &refs {
        assert!(session.read(reference).is_err());
    }
    let gc = session.store.clone();
    assert!(
        std::thread::spawn(move || gc.gc(Duration::ZERO).unwrap())
            .join()
            .unwrap()
            .blocked_by_staging
    );
    let pin = format!("transfer:partial-checkpoint:rpc:{}", Uuid::new_v4());
    session.clone_partial_pin(&stage.id, &pin, &refs).unwrap();
    assert!(!session.store.gc(Duration::ZERO).unwrap().blocked_by_staging);
    for reference in &refs {
        session.read(reference).unwrap();
    }
    session.finish().unwrap();
    session.store.gc(Duration::ZERO).unwrap();
    for reference in &refs {
        assert!(session.store.read_object(reference).is_err());
    }
}

#[test]
fn unknown_clone_generation_never_becomes_cleanup_authority_or_revives_consumed_stage() {
    let (_root, mut session) = fixture();
    let stage = session
        .store
        .begin_staging(Duration::from_secs(60))
        .unwrap();
    session.staging.insert(stage.id.clone());
    let reference = session
        .store
        .put_blob(b"ACK committed but reply lost")
        .unwrap();
    let refs = vec![reference.clone()];
    let pin = format!("transfer:partial-checkpoint:rpc:{}", Uuid::new_v4());
    pending(&mut session, &stage.id, &pin, &refs);
    let ack = session
        .store
        .retain_object_set(&stage.id, &pin, &refs)
        .unwrap();
    // Real ledger commit, but the caller did not receive/save its ACK.
    assert!(session.partial_pins[&pin].receipt.is_none());
    assert!(session.read(&reference).is_err());
    assert!(session.unpin_source(&pin).is_err());
    assert!(session.finish().is_err());
    assert_eq!(
        session.store.object_set_receipt(&ack.pin_id).unwrap(),
        Some(ack.clone())
    );
    assert!(session.store.retention().unwrap().staging.is_empty());
    let store = session.store.clone();
    drop(session);
    assert_eq!(store.object_set_refs(&ack).unwrap(), refs);
    assert!(store.retention().unwrap().staging.is_empty());
}

#[test]
fn pending_unknown_generation_never_releases_same_stage_aba_replacement() {
    let (_root, mut session) = fixture();
    let store = session.store.clone();
    let stage = store.begin_staging(Duration::from_secs(60)).unwrap();
    session.staging.insert(stage.id.clone());
    let refs = vec![store.put_blob(b"same-operation replacement bytes").unwrap()];
    let pin = format!("transfer:partial-checkpoint:rpc:{}", Uuid::new_v4());
    pending(&mut session, &stage.id, &pin, &refs);
    let first = store.retain_object_set(&stage.id, &pin, &refs).unwrap();
    // Real SQL ACK1 exists, but the failed final retention cleanup left its
    // original live stage. Reconstruct that precise conservative crash phase.
    let mut state = store.retention().unwrap();
    state.staging.insert(stage.id.clone(), stage.clone());
    state.generation += 1;
    let checksum = ObjectRef::from_bytes(ObjectKind::Blob, &canonical_json(&state).unwrap()).digest;
    std::fs::write(
        store.root().join("retention.json"),
        canonical_json(&json!({"state":state,"checksum":checksum})).unwrap(),
    )
    .unwrap();
    // These are public store operations. Reuse of the SAME original stage is
    // possible here, with identical pin/set identity and a newer generation.
    store.release_object_set(&first).unwrap();
    let second = store.retain_object_set(&stage.id, &pin, &refs).unwrap();
    assert!(second.generation > first.generation);
    assert_eq!(second.staging_id, first.staging_id);
    assert_eq!(second.set_digest, first.set_digest);
    assert!(!store.retention().unwrap().staging.contains_key(&stage.id));
    assert!(session.unpin_source(&pin).is_err());
    assert!(session.finish().is_err());
    assert_eq!(store.object_set_refs(&second).unwrap(), refs);
    assert!(store.release_object_set(&first).is_err());
    drop(session);
    assert_eq!(store.object_set_refs(&second).unwrap(), refs);
    assert!(!store.retention().unwrap().staging.contains_key(&stage.id));
}

#[test]
fn pending_cleanup_and_drop_preserve_foreign_replacement_and_uncertain_gc_fence() {
    let (_root, mut session) = fixture();
    let store = session.store.clone();
    let stage = store.begin_staging(Duration::from_secs(60)).unwrap();
    session.staging.insert(stage.id.clone());
    let reference = store.put_blob(b"uncertain clone bytes").unwrap();
    let refs = vec![reference];
    let pin = format!("transfer:partial-checkpoint:rpc:{}", Uuid::new_v4());
    pending(&mut session, &stage.id, &pin, &refs);
    let ack = store.retain_object_set(&stage.id, &pin, &refs).unwrap();
    store.release_object_set(&ack).unwrap();
    let foreign_stage = store.begin_staging(Duration::from_secs(60)).unwrap();
    let foreign = store
        .retain_object_set(&foreign_stage.id, &pin, &refs)
        .unwrap();
    // Restore only the exact conservative post-SQL/pre-retention-cleanup phase.
    let mut state = store.retention().unwrap();
    state.staging.insert(stage.id.clone(), stage.clone());
    state.generation += 1;
    let checksum = ObjectRef::from_bytes(ObjectKind::Blob, &canonical_json(&state).unwrap()).digest;
    std::fs::write(
        store.root().join("retention.json"),
        canonical_json(&json!({"state":state,"checksum":checksum})).unwrap(),
    )
    .unwrap();
    assert!(session.unpin_source(&pin).is_err());
    assert!(session.finish().is_err());
    assert!(store.retention().unwrap().staging.contains_key(&stage.id));
    drop(session);
    assert_eq!(store.object_set_refs(&foreign).unwrap(), refs);
    assert!(store.retention().unwrap().staging.contains_key(&stage.id));
    assert!(store.gc(Duration::ZERO).unwrap().blocked_by_staging);
}

#[test]
fn clone_limits_reject_before_any_read_admission_or_ledger_upgrade() {
    let (_root, mut session) = fixture();
    let stage = session
        .store
        .begin_staging(Duration::from_secs(60))
        .unwrap();
    session.staging.insert(stage.id.clone());
    let placeholder = ObjectRef::from_bytes(ObjectKind::Blob, b"no bytes needed");
    let mut too_large = placeholder.clone();
    too_large.size_bytes = TRANSFER_MAX_OBJECT_BYTES + 1;
    let too_many_bytes = (0u8..17)
        .map(|id| {
            let mut reference = ObjectRef::from_bytes(ObjectKind::Blob, &[id]);
            reference.size_bytes = TRANSFER_MAX_OBJECT_BYTES;
            reference
        })
        .collect::<Vec<_>>();
    for mut refs in [vec![placeholder; 16_385], vec![too_large], too_many_bytes] {
        refs.sort();
        let pin = format!("transfer:partial-checkpoint:rpc:{}", Uuid::new_v4());
        assert!(session.clone_partial_pin(&stage.id, &pin, &refs).is_err());
        assert!(session.partial_pins.is_empty());
    }
    assert_eq!(session.store.retention().unwrap().format_version, 1);
    session.finish().unwrap();
}
