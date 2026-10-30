use rudder_engine::{
    directory::{EntryNode, LocalDirectoryStore, PagedDirectory},
    protocol::{ObjectRef, OwnerEpoch},
    store::{ImportPolicy, LocalStore},
    vfs::{
        exclusion_policy_digest, DirtyCursor, NodeKind, ObservationBudget, ObservationConsistency,
        VfsManager, VfsView,
    },
};
use std::time::Duration;
use uuid::Uuid;

fn owner() -> OwnerEpoch {
    OwnerEpoch {
        owner: "observation-test".into(),
        epoch: 2,
    }
}
fn setup() -> (tempfile::TempDir, LocalStore, VfsManager, VfsView) {
    let base = std::env::var_os("RUDDER_TEST_TMPDIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            if std::path::Path::new("/workspace").is_dir() {
                "/workspace".into()
            } else {
                std::env::temp_dir()
            }
        });
    let temp = tempfile::Builder::new()
        .prefix("rudder-vfs-observation-")
        .tempdir_in(base)
        .unwrap();
    let store = LocalStore::open(temp.path().join("cas")).unwrap();
    let manager = VfsManager::open(temp.path().join("manager"), store.clone()).unwrap();
    let stage = store.begin_staging(Duration::from_secs(600)).unwrap();
    let adapter = LocalDirectoryStore::new(&store, &stage.id);
    let root = PagedDirectory::new(&adapter).build(0o755, []).unwrap();
    store.publish(&stage.id, "fixture", &root).unwrap();
    manager.prepare_paged(&root).unwrap();
    let view = manager
        .fork_paged(&Uuid::new_v4().to_string(), &root, owner())
        .unwrap();
    (temp, store, manager, view)
}
fn bytes(store: &LocalStore, root: &ObjectRef, name: &[u8]) -> Vec<u8> {
    let entry = rudder_engine::tree::lookup(store, root, name)
        .unwrap()
        .unwrap();
    let EntryNode::File {
        object,
        logical_size,
        ..
    } = entry.node
    else {
        panic!()
    };
    rudder_engine::content::read_file_range(store, &object, 0, logical_size as usize).unwrap()
}
fn connection(manager: &VfsManager, view: &VfsView) -> rusqlite::Connection {
    rusqlite::Connection::open(
        manager
            .root()
            .join("views")
            .join(view.id())
            .join("namespace.sqlite"),
    )
    .unwrap()
}

#[test]
fn source_observation_is_retained_repeatable_and_does_not_seal_or_clear_private_state() {
    let (_temp, store, manager, view) = setup();
    let start = view.dirty_cursor().unwrap();
    assert_eq!(start.generation, 0);
    let source = view
        .create(1, b"source", NodeKind::File, 0o644, b"")
        .unwrap();
    view.write(source.ino, 0, b"first", false).unwrap();
    let prepared = view
        .create(1, b"node_modules", NodeKind::Directory, 0o755, b"")
        .unwrap();
    let file = view
        .create(prepared.ino, b"dirty", NodeKind::File, 0o600, b"")
        .unwrap();
    view.set_attr(file.ino, None, Some(1u64 << 40), None, None)
        .unwrap();
    view.write(file.ino, (1u64 << 40) - 2, b"XX", false)
        .unwrap();
    let before_lookup = view.dirty_cursor().unwrap();
    view.lookup(1, b"source").unwrap();
    view.lookup(prepared.ino, b"dirty").unwrap();
    assert_eq!(view.dirty_cursor().unwrap(), before_lookup);
    let fd = view.open_handle(source.ino, libc::O_RDWR, false).unwrap();
    let policy = ImportPolicy {
        excluded_paths: vec![vec![b"node_modules".to_vec()]],
    };
    let budget = ObservationBudget {
        max_content_bytes: 100_000,
        ..ObservationBudget::default()
    };
    let nonce = Uuid::new_v4().to_string();
    let observed = view.observe_source(&policy, &nonce, &budget).unwrap();
    assert_eq!(observed.consistency, ObservationConsistency::LiveBestEffort);
    assert_eq!(observed.begin, observed.end);
    assert_eq!(observed.begin, before_lookup);
    assert_eq!(
        observed.exclusion_policy_digest,
        exclusion_policy_digest(&policy).unwrap()
    );
    assert_eq!(bytes(&store, &observed.directory, b"source"), b"first");
    assert!(
        rudder_engine::tree::lookup(&store, &observed.directory, b"node_modules")
            .unwrap()
            .is_none()
    );
    assert_eq!(
        store.retention().unwrap().pins.get(&observed.pin_id),
        Some(&observed.directory)
    );
    assert!(view.checkpoint_snapshot_unmounted().is_err()); //observation did not close our handle
    view.write(source.ino, 0, b"later", false).unwrap();
    assert_eq!(
        view.observe_source(&policy, &nonce, &budget).unwrap(),
        observed
    );
    assert!(view.dirty_cursor().unwrap().generation > observed.end.generation);
    store.gc(Duration::ZERO).unwrap();
    assert_eq!(bytes(&store, &observed.directory, b"source"), b"first");
    view.close_handle(source.ino, fd).unwrap();
    let sealed = view.checkpoint_snapshot_unmounted().unwrap();
    assert_eq!(bytes(&store, &sealed.directory, b"source"), b"later");
    let EntryNode::Directory { object } =
        rudder_engine::tree::lookup(&store, &sealed.directory, b"node_modules")
            .unwrap()
            .unwrap()
            .node
    else {
        panic!()
    };
    let EntryNode::File {
        object,
        logical_size,
        ..
    } = rudder_engine::tree::lookup(&store, &object, b"dirty")
        .unwrap()
        .unwrap()
        .node
    else {
        panic!()
    };
    assert_eq!(logical_size, 1u64 << 40);
    assert_eq!(
        rudder_engine::content::read_file_range(&store, &object, logical_size - 2, 2).unwrap(),
        b"XX"
    );
    view.release_observation(&nonce).unwrap();
    view.release_observation(&nonce).unwrap();
    assert!(!store
        .retention()
        .unwrap()
        .pins
        .contains_key(&observed.pin_id));
    assert!(view.observe_source(&policy, &nonce, &budget).is_err());
    assert!(manager.open_view(view.id(), &owner()).is_err()); //only the owning VfsView serves
}

#[test]
fn dirty_pages_have_owner_namespace_fencing_continuation_and_explicit_cursor_loss() {
    let (_temp, _store, manager, view) = setup();
    let start = view.dirty_cursor().unwrap();
    let file = view
        .create(1, b"raw-\xff", NodeKind::File, 0o600, b"")
        .unwrap();
    view.write(file.ino, 0, b"abc", false).unwrap();
    view.set_attr(file.ino, Some(0o640), None, None, None)
        .unwrap();
    let page = view.dirty_since(&start, 1).unwrap();
    assert_eq!(page.events.len(), 1);
    assert_eq!(page.events[0].name_hex.as_deref(), Some("7261772dff"));
    assert!(page.has_more);
    assert!(!page.complete);
    assert!(!page.cursor_lost);
    let rest = view.dirty_since(&page.next, 10).unwrap();
    assert_eq!(rest.events.len(), 2);
    assert!(rest.complete);
    assert!(!rest.has_more);
    assert_eq!(rest.next, rest.current);
    for wrong in [
        DirtyCursor {
            namespace: Uuid::new_v4().to_string(),
            ..start.clone()
        },
        DirtyCursor {
            owner: OwnerEpoch {
                epoch: 3,
                ..owner()
            },
            ..start.clone()
        },
        DirtyCursor {
            generation: u64::MAX,
            ..start.clone()
        },
    ] {
        let lost = view.dirty_since(&wrong, 10).unwrap();
        assert!(lost.cursor_lost);
        assert!(!lost.complete);
        assert!(lost.events.is_empty());
    }
    assert!(view.dirty_since(&start, 0).is_err());
    let c = connection(&manager, &view);
    c.execute("DELETE FROM dirty_events WHERE generation=1", [])
        .unwrap();
    let lost = view.dirty_since(&start, 10).unwrap();
    assert!(lost.cursor_lost);
    assert!(!lost.complete);
    c.execute("DELETE FROM dirty_events WHERE generation=3", [])
        .unwrap();
    assert!(view.dirty_since(&page.next, 10).is_err()); //a broken interior/tail is never a clean result
}

#[test]
fn publication_receipt_failure_recovers_original_observation_after_later_mutation() {
    let (_temp, store, manager, view) = setup();
    let file = view
        .create(1, b"source", NodeKind::File, 0o644, b"")
        .unwrap();
    view.write(file.ino, 0, b"first", false).unwrap();
    let before = view.dirty_cursor().unwrap();
    let nonce = Uuid::new_v4().to_string();
    let c = connection(&manager, &view);
    c.execute_batch("CREATE TRIGGER fail_observation BEFORE UPDATE ON observations WHEN NEW.status='complete' BEGIN SELECT RAISE(ABORT,'injected receipt failure'); END;").unwrap();
    assert!(view
        .observe_source(
            &ImportPolicy::default(),
            &nonce,
            &ObservationBudget::default()
        )
        .is_err());
    let pin = format!("vfs:observe:{}:{nonce}", view.id());
    let retained = store.retention().unwrap().pins[&pin].clone();
    assert_eq!(bytes(&store, &retained, b"source"), b"first");
    view.write(file.ino, 0, b"later", false).unwrap();
    c.execute_batch("DROP TRIGGER fail_observation;").unwrap();
    let recovered = view
        .observe_source(
            &ImportPolicy::default(),
            &nonce,
            &ObservationBudget::default(),
        )
        .unwrap();
    assert_eq!(recovered.directory, retained);
    assert_eq!(recovered.begin, before);
    assert_eq!(recovered.end, before);
    let policy = ImportPolicy {
        excluded_paths: vec![vec![b"other".to_vec()]],
    };
    assert!(view
        .observe_source(&policy, &nonce, &ObservationBudget::default())
        .is_err());
}

#[test]
fn failed_observation_is_budgeted_does_not_publish_and_does_not_claim_a_later_generation() {
    let (_temp, store, _manager, view) = setup();
    let file = view
        .create(1, b"source", NodeKind::File, 0o600, b"")
        .unwrap();
    view.write(file.ino, 0, b"abc", false).unwrap();
    let nonce = Uuid::new_v4().to_string();
    let budget = ObservationBudget {
        max_changed_entries: 0,
        ..ObservationBudget::default()
    };
    assert!(view
        .observe_source(&ImportPolicy::default(), &nonce, &budget)
        .unwrap_err()
        .to_string()
        .contains("budget"));
    assert!(!store
        .retention()
        .unwrap()
        .pins
        .contains_key(&format!("vfs:observe:{}:{nonce}", view.id())));
    assert!(store.retention().unwrap().staging.is_empty());
    view.write(file.ino, 0, b"def", false).unwrap();
    assert!(view
        .observe_source(
            &ImportPolicy::default(),
            &nonce,
            &ObservationBudget::default()
        )
        .unwrap_err()
        .to_string()
        .contains("uncertain"));
    let next = view
        .observe_source(
            &ImportPolicy::default(),
            &Uuid::new_v4().to_string(),
            &ObservationBudget::default(),
        )
        .unwrap();
    assert_eq!(bytes(&store, &next.directory, b"source"), b"def");
    assert!(view
        .release_observation(&Uuid::new_v4().to_string())
        .is_err());
}

#[test]
fn source_exclusions_are_canonical_bounded_raw_paths() {
    let first = ImportPolicy {
        excluded_paths: vec![
            vec![b"b".to_vec()],
            vec![b"a".to_vec(), b"deeper".to_vec()],
            vec![b"a".to_vec()],
            vec![b"a".to_vec()],
        ],
    };
    let second = ImportPolicy {
        excluded_paths: vec![vec![b"a".to_vec()], vec![b"b".to_vec()]],
    };
    assert_eq!(
        exclusion_policy_digest(&first).unwrap(),
        exclusion_policy_digest(&second).unwrap()
    );
    for path in [
        vec![],
        vec![b"..".to_vec()],
        vec![b"x/y".to_vec()],
        vec![vec![0]],
    ] {
        assert!(exclusion_policy_digest(&ImportPolicy {
            excluded_paths: vec![path]
        })
        .is_err());
    }
}
