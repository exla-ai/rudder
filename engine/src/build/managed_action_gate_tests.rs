//! Fail-closed action tests. The mutation probe uses host cc, not the managed
//! SDK; it demonstrates why matching before/after hashes cannot admit an action.
use super::*;
use crate::protocol::{CheckpointConsistency, Durability};
use crate::store::{
    CheckpointV2Manifest, DirectoryEntry, DirectoryManifest, EntryNode, SourceManifest,
};
use crate::workspace::{self, WorkspaceProfile};
use std::os::unix::fs::PermissionsExt;

struct Fixture {
    temporary: tempfile::TempDir,
    index: BuildIndex,
    prepared: PreparedEnvironment,
    input: BuildInputView,
    profile: BuildProfile,
    contract: ActionContract,
    sdk: ObjectRef,
    source: ObjectRef,
}

impl Fixture {
    fn new() -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let store = LocalStore::open(temporary.path().join("cas")).unwrap();
        let compiler = store.put_blob(b"compiler identity fixture").unwrap();
        let header = store.put_blob(b"#define SDK_VALUE 7\n").unwrap();
        let sdk = store
            .put_directory(DirectoryManifest {
                format_version: 1,
                mode: 0o755,
                entries: vec![
                    DirectoryEntry::new(
                        b"compiler",
                        EntryNode::File {
                            object: compiler.clone(),
                            mode: 0o555,
                        },
                    )
                    .unwrap(),
                    DirectoryEntry::new(
                        b"sdk.h",
                        EntryNode::File {
                            object: header,
                            mode: 0o444,
                        },
                    )
                    .unwrap(),
                ],
            })
            .unwrap();
        let platform = PlatformIdentity {
            os: "linux".into(),
            architecture: std::env::consts::ARCH.into(),
            abi: "test".into(),
            properties: BTreeMap::new(),
        };
        let toolchains = BTreeMap::from([(
            "compiler".into(),
            ToolchainIdentity {
                version: "1".into(),
                closure: sdk.clone(),
                executable: compiler,
                executable_path: "compiler".into(),
                configuration: BTreeMap::new(),
            },
        )]);
        let spec = PreparationSpec {
            format_version: 1,
            platform: platform.clone(),
            base_runtime: sdk.clone(),
            installer_inputs: BTreeMap::new(),
            installer_commands: vec![],
            installer_environment: BTreeMap::new(),
            toolchains: toolchains.clone(),
            policy_version: "test".into(),
            effects: BTreeSet::new(),
        };
        let index = BuildIndex::open(temporary.path().join("index"), store.clone()).unwrap();
        let prepared = index.publish_prepared(&spec, &sdk).unwrap();
        let content = store
            .put_blob(b"#include \"sdk.h\"\nint answer(void) { return SDK_VALUE; }\n")
            .unwrap();
        let source = store
            .put_directory(DirectoryManifest {
                format_version: 1,
                mode: 0o755,
                entries: vec![DirectoryEntry::new(
                    b"source.c",
                    EntryNode::File {
                        object: content,
                        mode: 0o444,
                    },
                )
                .unwrap()],
            })
            .unwrap();
        let source_root = store
            .put_source(&SourceManifest {
                format_version: 1,
                directory: source.clone(),
                parents: vec![],
                consistency: CheckpointConsistency::Quiesced,
            })
            .unwrap();
        let workspace_profile = store
            .put_blob(&canonical_json(&WorkspaceProfile::source_only()).unwrap())
            .unwrap();
        let checkpoint = store
            .put_checkpoint_v2(&CheckpointV2Manifest {
                format_version: 2,
                source: source_root.clone(),
                environment: Some(prepared.root.clone()),
                view_root: source.clone(),
                profile: workspace_profile,
                parents: vec![],
                knowledge_cursor: 0,
                consistency: CheckpointConsistency::Quiesced,
                durability: Durability::Local,
            })
            .unwrap();
        let input = workspace::build_input_view(&store, &checkpoint).unwrap();
        let mut profile =
            BuildProfile::from_toml(include_str!("../../../examples/build-profile/rudder.toml"))
                .unwrap();
        profile.platform = platform.clone();
        profile.buck2.executable_sha256 = hash_file(Path::new("/usr/bin/true")).unwrap();
        profile.buck2.remote_cache = None;
        let contract = ActionContract {
            format_version: 1,
            frontend: "buck2-rudder-linux-compile-v1".into(),
            frontend_executable: store.put_blob(b"frontend fixture").unwrap(),
            rules: store.put_blob(b"rules fixture").unwrap(),
            source_root,
            input_topology: None,
            environment_root: prepared.root.clone(),
            platform,
            toolchains,
            command: vec!["buck2".into(), "build".into()],
            environment: BTreeMap::new(),
            outputs: BTreeSet::from(["result".into()]),
            policy: profile.policy.clone(),
        };
        Self {
            temporary,
            index,
            prepared,
            input,
            profile,
            contract,
            sdk,
            source,
        }
    }

    fn qualified(&self, outputs: ObjectRef) -> QualifiedBuild {
        let log = self
            .index
            .store()
            .put_blob(b"successful frontend report fixture")
            .unwrap();
        QualifiedBuild {
            contract: self.contract.clone(),
            prepared: self.prepared.clone(),
            outputs,
            observation: FrontendObservation {
                frontend: self.contract.frontend.clone(),
                trace_id: "fixture".into(),
                executed_actions: 1,
                remote_cache_hits: 0,
                native_action_digests: vec![],
                event_log: log.clone(),
                build_report: log,
            },
        }
    }
}

#[test]
fn managed_action_gate_refuses_local_and_remote_profiles_before_materialization() {
    let mut f = Fixture::new();
    let adapter = Buck2Adapter::new("/usr/bin/true").unwrap();
    let before = f.index.store().retention().unwrap().pins;
    for remote in [false, true] {
        f.profile.buck2.remote_cache = remote.then(|| RemoteCacheConfig {
            cas_address: "grpc://127.0.0.1:1".into(),
            action_cache_address: "grpc://127.0.0.1:1".into(),
            instance_name: "must-not-contact".into(),
            tls: false,
        });
        let workspace = f.temporary.path().join(format!("refused-{remote}"));
        let error = adapter
            .build(&f.index, &f.profile, &f.input, &f.prepared, &workspace)
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("writer exclusion is not enforced"),
            "{error:#}"
        );
        assert!(!workspace.exists());
        assert!(!f.index.root().join("materialized").exists());
        assert_eq!(
            f.index.store().retention().unwrap().pins,
            before,
            "refusal must release the input pin"
        );
    }
    assert_eq!(
        f.index.prepared_root(&f.prepared.root).unwrap(),
        Some(f.prepared.clone())
    );
}

#[test]
fn managed_action_gate_ignores_old_retained_results_and_denies_new_publication() {
    let f = Fixture::new();
    let store = f.index.store();
    let qualified = f.qualified(f.source.clone());
    let key = f.contract.key(store).unwrap();
    let receipt = metadata(store, ObjectKind::ActionResult, "frontend-build-receipt-v1",
        &serde_json::json!({"key":key,"outputs":qualified.outputs,"observation":qualified.observation}),
        vec![key.clone(), qualified.outputs.clone(), qualified.observation.event_log.clone()]).unwrap();
    // Reproduce an old fully retained index entry without using current admission.
    let old_pin = format!("build/action/{}", key.digest);
    store.pin(&old_pin, &receipt).unwrap();
    let connection = rusqlite::Connection::open(f.index.root().join("index.sqlite")).unwrap();
    connection
        .execute(
            "INSERT INTO build_index VALUES (?1,'action',?2,?3,?4)",
            rusqlite::params![
                1,
                key.digest,
                serde_json::to_string(&key).unwrap(),
                serde_json::to_string(&receipt).unwrap()
            ],
        )
        .unwrap();
    let before = store.retention().unwrap().pins;
    assert!(
        f.index.lookup_build(&f.contract).unwrap().is_none(),
        "old retained output is not lifetime custody"
    );
    let error = f.index.record_build(&qualified).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("writer exclusion is not enforced"),
        "{error:#}"
    );
    assert_eq!(
        store.retention().unwrap().pins,
        before,
        "do not publish or discard recovery roots"
    );
    let count: i64 = connection
        .query_row(
            "SELECT count(*) FROM build_index WHERE category='action'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn managed_action_gate_rejects_mutate_during_execution_and_restore_for_source_and_sdk() {
    let f = Fixture::new();
    let workspace = f.temporary.path().join("mutation-probe");
    fs::create_dir(&workspace).unwrap();
    sync_inputs(f.index.store(), &f.source, &workspace).unwrap();
    let sdk = materialize_environment(&f.index, &f.sdk).unwrap();
    let source = workspace.join("inputs/source.c");
    let baseline = workspace.join("baseline.o");
    let mut cc = Command::new("/usr/bin/cc");
    cc.arg("-c")
        .arg(&source)
        .arg("-I")
        .arg(&sdk)
        .arg("-o")
        .arg(&baseline);
    bounded(
        &mut cc,
        Duration::from_secs(10),
        &workspace.join("stdout"),
        &workspace.join("stderr"),
    )
    .unwrap();
    let output = workspace.join("outputs");
    fs::create_dir(&output).unwrap();
    for (input, mutated) in [
        (
            source.clone(),
            "#include \"sdk.h\"\nint answer(void) { return SDK_VALUE + 1; }\n",
        ),
        (sdk.join("sdk.h"), "#define SDK_VALUE 9\n"),
    ] {
        let original = fs::read_to_string(&input).unwrap();
        // A previously permitted writer retains an open descriptor even after
        // chmod. The action sees transient bytes, then both verifiers pass.
        fs::set_permissions(&input, fs::Permissions::from_mode(0o644)).unwrap();
        let writer = OpenOptions::new().write(true).open(&input).unwrap();
        fs::set_permissions(&input, fs::Permissions::from_mode(0o444)).unwrap();
        verify_directory(f.index.store(), &f.source, &workspace.join("inputs")).unwrap();
        verify_directory(f.index.store(), &f.sdk, &sdk).unwrap();
        let artifact = output.join("changed.o");
        let mut child = Command::new("/usr/bin/python3");
        child
            .args([
                "-I",
                "-c",
                r#"
import os, subprocess, sys
mutated, original, source, sdk, output = sys.argv[1:]
def replace(value):
    data = value.encode()
    os.pwrite(0, data, 0)
    os.ftruncate(0, len(data))
    os.fsync(0)
try:
    replace(mutated)
    subprocess.run(['/usr/bin/cc', '-c', source, '-I', sdk, '-o', output], check=True, timeout=5)
finally:
    replace(original)
"#,
            ])
            .args([mutated, &original])
            .arg(&source)
            .arg(&sdk)
            .arg(&artifact)
            .stdin(Stdio::from(writer));
        bounded(
            &mut child,
            Duration::from_secs(10),
            &workspace.join("stdout"),
            &workspace.join("stderr"),
        )
        .unwrap();
        assert_ne!(
            fs::read(&baseline).unwrap(),
            fs::read(&artifact).unwrap(),
            "actual compiler consumed the transient mutation"
        );
        verify_directory(f.index.store(), &f.source, &workspace.join("inputs")).unwrap();
        verify_directory(f.index.store(), &f.sdk, &sdk).unwrap();
        let outputs = f
            .index
            .store()
            .import_tree(&output, CheckpointConsistency::Quiesced)
            .unwrap();
        let qualified = f.qualified(outputs);
        let before = f.index.store().retention().unwrap().pins;
        let error = f.index.record_build(&qualified).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("writer exclusion is not enforced"),
            "{error:#}"
        );
        assert!(f.index.lookup_build(&f.contract).unwrap().is_none());
        assert_eq!(f.index.store().retention().unwrap().pins, before);
    }
}
