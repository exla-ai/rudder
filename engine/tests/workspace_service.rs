use rudder_engine::{
    paths::FabricPaths,
    protocol::{ObjectKind, ObjectRef, FORMAT_VERSION},
    service::{handle_store, StoreOperation, StoreRequest},
    store::{DirectoryEntry, EntryNode, LocalStore, SourceManifest},
};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;

fn call(paths: &FabricPaths, request: StoreOperation) -> Value {
    handle_store(paths, StoreRequest { format_version: FORMAT_VERSION, request }).unwrap()["result"].clone()
}
fn root(value: &Value) -> ObjectRef { serde_json::from_value(value["root"].clone()).unwrap() }

#[test]
fn prepared_checkpoint_survives_loss_of_the_original_store() {
    let fixture = tempfile::tempdir().unwrap();
    let paths = FabricPaths::new(&fixture.path().join("home"));
    let source = fixture.path().join("source");
    let prepared = fixture.path().join("prepared");
    fs::create_dir(&source).unwrap();
    fs::create_dir(&prepared).unwrap();
    fs::write(source.join("main.ts"), b"console.log('base');\n").unwrap();
    fs::write(prepared.join("dependency.js"), b"prepared-once").unwrap();
    let base = root(&call(&paths, StoreOperation::ImportSource { path: source, writers_quiesced: true, excluded_paths: vec![], parents: vec![], pin: "source:base".into() }));
    let mut platform = BTreeMap::new();
    platform.insert("os".into(), "linux".into());
    platform.insert("abi".into(), "glibc-2.36-x86_64".into());
    let environment = root(&call(&paths, StoreOperation::PrepareEnvironment { path: prepared, writers_quiesced: true, platform, installer_inputs: vec![], provenance: BTreeMap::from([("image".into(), "sha256:fixture".into())]), pin: "env:base".into() }));
    let checkpoint = root(&call(&paths, StoreOperation::Checkpoint { source: base.clone(), environment: Some(environment), parents: vec![], knowledge_cursor: 42, pin: "checkpoint:base".into() }));
    let replica = fixture.path().join("other-worker-volume");
    let copied = call(&paths, StoreOperation::TransferLocal { root: checkpoint.clone(), destination_store: replica.clone(), pin: "replica:checkpoint".into() });
    assert!(copied["transferred_bytes"].as_u64().unwrap() > 0);
    assert_eq!(copied["durability"], "local-mirror");
    let repeated = call(&paths, StoreOperation::TransferLocal { root: checkpoint.clone(), destination_store: replica.clone(), pin: "replica:checkpoint".into() });
    assert_eq!(repeated["transferred_objects"], 0);
    fs::remove_dir_all(&paths.cas).unwrap();
    let store = LocalStore::open(replica).unwrap();
    assert_eq!(store.closure(&checkpoint).unwrap().len(), copied["verified_objects"].as_u64().unwrap() as usize);
    let source_manifest: SourceManifest = serde_json::from_slice(&store.read_object(&base).unwrap()).unwrap();
    let restored = fixture.path().join("restored");
    store.export_tree(&source_manifest.directory, &restored).unwrap();
    assert_eq!(fs::read(restored.join("main.ts")).unwrap(), b"console.log('base');\n");
}

#[test]
fn source_updates_reuse_immutable_ancestors_and_keep_siblings_private() {
    let fixture = tempfile::tempdir().unwrap();
    let paths = FabricPaths::new(&fixture.path().join("home"));
    let input = fixture.path().join("input");
    fs::create_dir(&input).unwrap();
    fs::write(input.join("file"), b"original").unwrap();
    let base = root(&call(&paths, StoreOperation::ImportSource { path: input, writers_quiesced: true, excluded_paths: vec![], parents: vec![], pin: "base".into() }));
    let store = LocalStore::open(&paths.cas).unwrap();
    let blob = store.put_blob(b"changed").unwrap();
    let child = root(&call(&paths, StoreOperation::UpdateSource { source: base.clone(), path: vec![b"file".to_vec()], replacement: Some(DirectoryEntry::new(b"file", EntryNode::File { object: blob, mode: 0o644 }).unwrap()), pin: "child".into() }));
    assert_ne!(base, child);
    for (name, reference, expected) in [("parent", &base, b"original".as_slice()), ("sibling", &base, b"original".as_slice()), ("child", &child, b"changed".as_slice())] {
        let manifest: SourceManifest = serde_json::from_slice(&store.read_object(reference).unwrap()).unwrap();
        let export = fixture.path().join(name);
        call(&paths, StoreOperation::ExportTree { directory: manifest.directory, destination: export.clone() });
        assert_eq!(fs::read(export.join("file")).unwrap(), expected);
    }
    fs::write(fixture.path().join("child/file"), b"private write").unwrap();
    assert_eq!(fs::read(fixture.path().join("sibling/file")).unwrap(), b"original");
    let child_manifest: SourceManifest = serde_json::from_slice(&store.read_object(&child).unwrap()).unwrap();
    assert_eq!(child_manifest.parents, vec![base]);
    assert_eq!(store.read_directory(&child_manifest.directory).unwrap().entries[0].node, EntryNode::File { object: ObjectRef::from_bytes(ObjectKind::Blob, b"changed"), mode: 0o644 });
}

#[test]
fn service_rejects_unsupported_version_and_unbarriered_or_partial_publication() {
    let fixture = tempfile::tempdir().unwrap();
    let paths = FabricPaths::new(fixture.path());
    assert!(handle_store(&paths, StoreRequest { format_version: 2, request: StoreOperation::Capabilities }).is_err());
    assert!(handle_store(&paths, StoreRequest { format_version: 1, request: StoreOperation::ImportSource { path: fixture.path().to_path_buf(), writers_quiesced: false, excluded_paths: vec![], parents: vec![], pin: "bad".into() } }).is_err());
    let missing = ObjectRef::from_bytes(ObjectKind::SourceRoot, b"missing");
    assert!(handle_store(&paths, StoreRequest { format_version: 1, request: StoreOperation::Checkpoint { source: missing, environment: None, parents: vec![], knowledge_cursor: 0, pin: "partial".into() } }).is_err());
    assert_eq!(call(&paths, StoreOperation::Retention)["pins"], json!({}));
}
