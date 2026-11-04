use super::*;
use crate::build::sealed_inputs::{tests::fixture, SealedBuildInputs};
use std::{os::unix::process::CommandExt, process::{Child, Stdio}};

struct OriginalChild(Child);
impl Drop for OriginalChild {
    fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); }
}
fn dead_owner() -> ProcessIdentity {
    let mut child = Command::new("/usr/bin/true").spawn().unwrap();
    let identity = ProcessIdentity::capture(child.id()).unwrap();
    child.wait().unwrap();
    identity
}
fn retained(index: &BuildIndex, prepared: &PreparedEnvironment, input: &BuildInputView, workspace: &Path) -> InflightBuild {
    fs::create_dir(workspace).unwrap();
    fs::write(workspace.join(".rudder-managed-build.json"), b"{}").unwrap();
    let mut sealed = SealedBuildInputs::acquire(index, input, prepared).unwrap();
    let mut original = InflightBuild::new(input, &prepared.root, sealed.pin_data()).unwrap();
    sealed.mark_inflight();
    original.owner = dead_owner();
    original.driver = Some(dead_owner());
    original.save(workspace).unwrap();
    original
}
fn assert_retained(index: &BuildIndex, original: &InflightBuild, workspace: &Path) {
    assert_eq!(index.store().retention().unwrap().pins.get(&original.retained_input_pin), Some(&original.checkpoint));
    assert!(workspace.join(".rudder-build-inflight").is_file());
}

#[test]
fn recovery_stops_actual_original_driver_and_releases_only_its_exact_pin() {
    for explicit_stop in [false, true] {
    let (temp, index, prepared, input) = fixture();
    let workspace = temp.path().join("original");
    let mut original = retained(&index, &prepared, &input, &workspace);
    let replacement = SealedBuildInputs::acquire(&index, &input, &prepared).unwrap();
    let mut command = Command::new("/usr/bin/python3");
    command.args(["-I", "-c", "import time; time.sleep(30)"]).process_group(0).stdout(Stdio::null()).stderr(Stdio::null());
    let mut driver = OriginalChild(command.spawn().unwrap());
    original.driver = Some(ProcessIdentity::capture(driver.0.id()).unwrap());
    original.save(&workspace).unwrap();
    let adapter = Buck2Adapter::new("/usr/bin/true").unwrap();
    if explicit_stop {
        adapter.stop(&workspace).unwrap();
        assert_retained(&index, &original, &workspace);
    }
    adapter.reconcile_inflight(&index, &workspace).unwrap();
    assert!(!driver.0.wait().unwrap().success(), "recovery must stop the original driver, without launching another build");
    let pins = index.store().retention().unwrap().pins;
    assert!(!pins.contains_key(&original.retained_input_pin));
    assert_eq!(pins.get(replacement.pin_data()), Some(input.checkpoint()));
    assert!(!workspace.join(".rudder-build-inflight").exists());
    }
}

#[test]
fn successful_stop_command_cannot_replace_actual_detached_daemon_termination() {
    let (temp, index, prepared, input) = fixture();
    let workspace = temp.path().join("original");
    let mut original = retained(&index, &prepared, &input, &workspace);
    // setsid models Buck2's daemon leaving the original driver's group.
    let ready = workspace.join("daemon-ready");
    let mut daemon = OriginalChild(Command::new("/usr/bin/python3")
        .args(["-I", "-c", "import os,pathlib,sys,time; os.setsid(); pathlib.Path(sys.argv[1]).write_text('ready'); time.sleep(30)"])
        .arg(&ready).env_clear().env("HOME", workspace.join(".driver/home"))
        .env("RUDDER_BUILD_ATTEMPT", "prior-successful-warm-attempt").spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(3);
    while !ready.exists() { assert!(Instant::now() < deadline); std::thread::sleep(Duration::from_millis(10)); }
    let error = original.finish(&index, &workspace, Path::new("/usr/bin/true")).unwrap_err();
    assert!(format!("{error:#}").contains("daemon remains live"), "{error:#}");
    assert_retained(&index, &original, &workspace);
    assert!(original.daemons.iter().any(|id|id.pid == daemon.0.id() as i32));
    daemon.0.kill().unwrap(); daemon.0.wait().unwrap();
    InflightBuild::recovery(&index, &workspace, Path::new("/usr/bin/true")).unwrap();
    assert!(!index.store().retention().unwrap().pins.contains_key(&original.retained_input_pin));
}

#[test]
fn live_original_owner_and_legacy_missing_identity_never_release_custody() {
    let (temp, index, prepared, input) = fixture();
    let workspace = temp.path().join("original");
    let mut original = retained(&index, &prepared, &input, &workspace);
    original.owner = ProcessIdentity::capture(std::process::id()).unwrap();
    original.save(&workspace).unwrap();
    assert!(InflightBuild::recovery(&index, &workspace, Path::new("/usr/bin/true")).unwrap_err().to_string().contains("still live"));
    assert_retained(&index, &original, &workspace);
    let legacy = canonical_json(&serde_json::json!({"attempt":"legacy","retained_input_pin":original.retained_input_pin,"checkpoint":original.checkpoint})).unwrap();
    fs::write(workspace.join(".rudder-build-inflight"), &legacy).unwrap();
    assert!(InflightBuild::recovery(&index, &workspace, Path::new("/usr/bin/true")).is_err());
    assert_eq!(fs::read(workspace.join(".rudder-build-inflight")).unwrap(), legacy);
    assert_retained(&index, &original, &workspace);
}

#[test]
fn changed_original_pin_and_corrupt_original_checkpoint_are_not_released() {
    for corrupt in [false, true] {
        let (temp, index, prepared, input) = fixture();
        let workspace = temp.path().join("original");
        let mut original = retained(&index, &prepared, &input, &workspace);
        let other = index.store().put_blob(b"different custody").unwrap();
        if corrupt {
            let path = index.store().object_path(input.checkpoint()).unwrap();
            fs::remove_file(&path).unwrap();
            fs::write(path, b"corrupt original checkpoint").unwrap();
        } else {
            index.store().pin(&original.retained_input_pin, &other).unwrap();
        }
        assert!(original.finish(&index, &workspace, Path::new("/usr/bin/true")).is_err());
        assert!(workspace.join(".rudder-build-inflight").exists());
        assert_eq!(index.store().retention().unwrap().pins.get(&original.retained_input_pin), Some(if corrupt { input.checkpoint() } else { &other }));
    }
}

fn qualified(index: &BuildIndex, prepared: &PreparedEnvironment, input: &BuildInputView) -> QualifiedBuild {
    let spec = index.preparation_spec(prepared).unwrap();
    let environment = environment_content(index.store(), &prepared.root).unwrap();
    let blob = index.store().put_blob(b"original frontend evidence").unwrap();
    QualifiedBuild { contract: ActionContract { format_version: FORMAT_VERSION, frontend: "buck2-rudder-linux-compile-v1".into(),
        frontend_executable: blob.clone(), rules: blob.clone(), source_root: input.source_root().clone(), input_topology: None,
        environment_root: prepared.root.clone(), platform: spec.platform, toolchains: spec.toolchains, command: vec!["compile".into()],
        environment: BTreeMap::new(), outputs: BTreeSet::from(["library".into()]),
        policy: BuildProfile::from_toml(include_str!("../../../examples/build-profile/rudder.toml")).unwrap().policy },
        prepared: prepared.clone(), outputs: environment.directory,
        observation: FrontendObservation { frontend: "buck2-rudder-linux-compile-v1".into(), trace_id: uuid::Uuid::new_v4().to_string(),
            executed_actions: 1, remote_cache_hits: 0, native_action_digests: vec![], event_log: blob.clone(), build_report: blob } }
}

#[test]
fn cancellation_losing_publication_race_reconciles_the_original_published_action() {
    let (temp, index, prepared, input) = fixture();
    let workspace = temp.path().join("original");
    let mut original = retained(&index, &prepared, &input, &workspace);
    let build = qualified(&index, &prepared, &input);
    original.publication = Some(Publication { contract: build.contract.clone(), outputs: build.outputs.clone(), trace_id: build.observation.trace_id.clone(), run_receipt: None });
    original.save(&workspace).unwrap();
    let control = BuildExecutionControl::new(Duration::from_secs(10)).unwrap();
    let stored = control.publish(|| index.record_build(&build)).unwrap();
    assert!(!control.cancel().unwrap());
    assert!(control.publication_started().unwrap());
    original.finish(&index, &workspace, Path::new("/usr/bin/true")).unwrap();
    assert_eq!(original.reconciliation_data().unwrap()["action"], serde_json::to_value(&stored.receipt).unwrap());
    assert_eq!(index.lookup_build(&build.contract).unwrap(), Some(stored));
    assert!(!index.store().retention().unwrap().pins.contains_key(&original.retained_input_pin));
}

#[test]
fn partial_storage_publication_keeps_original_custody_and_records_actual_pins() {
    let (temp, index, prepared, input) = fixture();
    let workspace = temp.path().join("original");
    let mut original = retained(&index, &prepared, &input, &workspace);
    let build = qualified(&index, &prepared, &input);
    let key = build.contract.key(index.store()).unwrap();
    let receipt = metadata(index.store(), ObjectKind::ActionResult, "frontend-build-receipt-v1",
        &serde_json::json!({"key":key,"outputs":build.outputs,"observation":build.observation}),
        vec![key.clone(), build.outputs.clone(), build.observation.event_log.clone(), build.observation.build_report.clone()]).unwrap();
    // record_build pins before its SQL transaction. Model the original write
    // committing followed by a storage error before index acknowledgment.
    index.store().pin(&format!("build/action/{}", key.digest), &receipt).unwrap();
    original.publication = Some(Publication { contract: build.contract, outputs: build.outputs, trace_id: build.observation.trace_id, run_receipt: None });
    original.save(&workspace).unwrap();
    let error = original.finish(&index, &workspace, Path::new("/usr/bin/true")).unwrap_err();
    assert!(error.to_string().contains("partial"), "{error:#}");
    assert_retained(&index, &original, &workspace);
    let recorded: InflightBuild = serde_json::from_slice(&fs::read(workspace.join(".rudder-build-inflight")).unwrap()).unwrap();
    assert_eq!(recorded.reconciliation_data().unwrap()["action"], serde_json::to_value(&receipt).unwrap());
    assert!(recorded.reconciliation_data().unwrap()["indexed"].is_null());
}

#[test]
fn verified_success_transfers_exact_checkpoint_to_run_custody_without_stopping_warm_daemon() {
    let (temp, index, prepared, input) = fixture();
    let workspace = temp.path().join("original");
    let mut original = retained(&index, &prepared, &input, &workspace);
    let build = qualified(&index, &prepared, &input);
    let stored = index.record_build(&build).unwrap();
    let receipt = metadata(index.store(), ObjectKind::ActionResult, "managed-build-run-v1",
        &serde_json::json!({"stored":stored.receipt,"observation":build.observation,"input_view":input}),
        vec![stored.receipt.clone(), input.checkpoint().clone()]).unwrap();
    index.store().pin(&format!("build/run/{}", build.observation.trace_id), &receipt).unwrap();
    original.publication = Some(Publication { contract: build.contract.clone(), outputs: build.outputs.clone(), trace_id: build.observation.trace_id.clone(), run_receipt: Some(receipt.clone()) });
    original.save(&workspace).unwrap();
    let mut daemon = OriginalChild(Command::new("/usr/bin/python3")
        .args(["-I", "-c", "import time; time.sleep(30)"]).env_clear().env("HOME", workspace.join(".driver/home")).spawn().unwrap());
    let run = BuildRun { format_version: FORMAT_VERSION, stored, run_receipt: receipt, observation: build.observation,
        frontend_graph_reused_actions: 0, frontend_elapsed_ms: 1, environment_materialization_ms: 1, source_materialization_ms: 1, stage_ms: BTreeMap::new() };
    original.finish_published(&index, &workspace, &run).unwrap();
    assert!(!original.processes_settled, "a success transfer cannot claim daemon termination");
    assert!(daemon.0.try_wait().unwrap().is_none(), "warm daemon must survive verified success");
    assert!(!index.store().retention().unwrap().pins.contains_key(&original.retained_input_pin));
    index.store().gc(Duration::ZERO).unwrap();
    super::super::sealed_inputs::qualified_input(index.store(), input.checkpoint()).unwrap();
    assert!(!workspace.join(".rudder-build-inflight").exists());
}

#[test]
fn custody_release_compares_checkpoint_under_retention_lock() {
    let (_temp, index, prepared, input) = fixture();
    let owner = SealedBuildInputs::acquire(&index, &input, &prepared).unwrap();
    let changed = index.store().put_blob(b"replacement pin target").unwrap();
    index.store().pin(owner.pin_data(), &changed).unwrap();
    assert!(index.store().release_build_input_pin(owner.pin_data(), input.checkpoint()).is_err());
    assert_eq!(index.store().retention().unwrap().pins.get(owner.pin_data()), Some(&changed));
}
