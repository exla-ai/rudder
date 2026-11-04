//! Real native framing and lossless receipt handling. Graph authorization is
//! qualified separately by engine/tests/remote_root.rs; this fixture creates
//! retained data directly and never grants a live remote session.
use rudder_engine::{
    delta::{DeltaPlan, PartialTreeReceipt},
    paths::FabricPaths,
    protocol::{canonical_json, ObjectKind, ObjectRef, OwnerEpoch},
    remote_cache::CacheScope,
    remote_root::{AttemptBinding, RootBinding},
    store::LocalStore,
};
use serde_json::{json, Value};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::Duration,
};
use uuid::Uuid;

struct Fixture(PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn call(child: &mut Child, output: &mut impl BufRead, operation: Value) -> (Value, Vec<u8>) {
    let request = json!({"format_version":1,"id":Uuid::new_v4().to_string(),"request":operation,"binary_bytes":0});
    writeln!(child.stdin.as_mut().unwrap(), "{request}").unwrap();
    child.stdin.as_mut().unwrap().flush().unwrap();
    let mut line = String::new();
    assert!(output.read_line(&mut line).unwrap() > 0);
    let header: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(header["id"], request["id"]);
    let mut bytes = vec![0; header["binary_bytes"].as_u64().unwrap().try_into().unwrap()];
    output.read_exact(&mut bytes).unwrap();
    (header, bytes)
}

#[test]
fn actual_rpc_clones_partial_retention_and_preserves_generations_above_js_integer_range() {
    let fixture =
        Fixture(std::env::temp_dir().join(format!("rudder-partial-wire-{}", Uuid::new_v4())));
    fs::create_dir(&fixture.0).unwrap();
    fs::set_permissions(&fixture.0, fs::Permissions::from_mode(0o700)).unwrap();
    let paths = FabricPaths::new(&fixture.0);
    let store = LocalStore::open(&paths.cas).unwrap();
    // Initialize the ledger, then advance its protected sequence to represent
    // a valid long-lived host whose u64 generation JS cannot represent exactly.
    let first = store.begin_staging(Duration::from_secs(60)).unwrap();
    let seed = store.put_blob(b"seed").unwrap();
    let initial = store.retain_object_set(&first.id, "seed", &[seed]).unwrap();
    store.release_object_set(&initial).unwrap();
    let db = rusqlite::Connection::open(paths.cas.join("object-sets.sqlite")).unwrap();
    db.execute(
        "UPDATE object_set_meta SET next_generation=? WHERE id=1",
        [9_007_199_254_740_993i64],
    )
    .unwrap();
    drop(db);
    let stage = store.begin_staging(Duration::from_secs(60)).unwrap();
    let plan = DeltaPlan {
        format_version: 1,
        capture_id: Uuid::new_v4().to_string(),
        base: RootBinding {
            scope: CacheScope {
                provider_id: "fixture".into(),
                tenant_id: "tenant".into(),
            },
            root_id: "base".into(),
            root: ObjectRef::from_bytes(ObjectKind::RunnableRoot, b"not hydrated"),
        },
        attempt: AttemptBinding {
            project_id: "project".into(),
            run_id: "run".into(),
            run_epoch: u64::MAX,
            owner: OwnerEpoch {
                owner: "owner".into(),
                epoch: u64::MAX,
            },
            execution_id: Uuid::new_v4().to_string(),
            operation_id: Uuid::new_v4().to_string(),
            view_id: Uuid::new_v4().to_string(),
        },
        directory: ObjectRef::from_bytes(ObjectKind::Directory, b"inherited directory"),
        hardlinks: ObjectRef::from_bytes(ObjectKind::Blob, b"inherited topology"),
        local_objects: vec![],
        remote_boundaries: vec![],
    };
    let encoded = canonical_json(&plan).unwrap();
    let plan_ref = store.put_blob(&encoded).unwrap();
    let retained = store
        .retain_object_set(&stage.id, "producer", &[plan_ref.clone()])
        .unwrap();
    assert_eq!(retained.generation, 9_007_199_254_740_993);
    let receipt = PartialTreeReceipt {
        format_version: 1,
        plan: plan_ref.clone(),
        local: retained.clone(),
    };
    let receipt_json = serde_json::to_string(&receipt).unwrap();
    let mut process = Process(
        Command::new(env!("CARGO_BIN_EXE_rudder-native"))
            .args(["fabric", "rpc"])
            .env("RUDDER_HOME", &fixture.0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let mut output = BufReader::new(process.0.stdout.take().unwrap());
    let (response, _) = call(
        &mut process.0,
        &mut output,
        json!({"operation":"pin_partial_source","receipt_json":receipt_json}),
    );
    assert_eq!(response["ok"], true, "{response}");
    let pin = response["result"]["owned_pin"].as_str().unwrap().to_owned();
    assert!(pin.starts_with("transfer:partial:rpc:"));
    assert_eq!(response["result"]["objects"], json!([plan_ref]));
    store.release_object_set(&retained).unwrap();
    store.gc(Duration::ZERO).unwrap();
    let (response, bytes) = call(
        &mut process.0,
        &mut output,
        json!({"operation":"read_binary","object":plan_ref}),
    );
    assert_eq!(response["ok"], true);
    assert_eq!(bytes, encoded);
    let (denied, _) = call(
        &mut process.0,
        &mut output,
        json!({"operation":"unpin_source","pin":"producer"}),
    );
    assert_eq!(denied["ok"], false);
    let (denied, _) = call(
        &mut process.0,
        &mut output,
        json!({"operation":"inventory","root":plan.directory}),
    );
    assert_eq!(denied["ok"], false);
    drop(process.0.stdin.take());
    assert!(process.0.wait().unwrap().success());
    assert!(store.object_set_receipt(&pin).unwrap().is_none());
    store.gc(Duration::ZERO).unwrap();
    assert!(store.read_object(&plan_ref).is_err());
}
