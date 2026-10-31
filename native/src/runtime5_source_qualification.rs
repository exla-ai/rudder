//! PRIVATE opt-in actual source5 composition. DATA inputs never restore authority.
use super::*;
use crate::source_shutdown_join::ExitedSourceShutdown;
use rudder_engine::{
    execution::{
        handoff::{ManagedExecutionEvent, ManagedExecutionHandoff},
        CheckpointRef, StopAndCheckpoint,
    },
    remote_root::RootBinding,
};
use rudder_native::pty_terminal::{TerminalCommand, TerminalPane, TerminalPaneOptions};

#[path = "runtime5_checkpoint_transfer.rs"]
mod checkpoint_transfer;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Graph {
    root: RootBinding,
    profile: ObjectRef,
    accepted_source: ObjectRef,
    accepted_seed: ObjectRef,
    projection_policy: ObjectRef,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    format_version: u32,
    native: PathBuf,
    node: PathBuf,
    cli_script: PathBuf,
    base_url: String,
    auth_headers: BTreeMap<String, String>,
    graph: Graph,
    proof: PathBuf,
    #[serde(default)]
    checkpoint_transfer: Option<checkpoint_transfer::Input>,
}

const GUEST: &str = r#"import json,mmap,os,socket,sys,time
assert os.getuid()==1500 and os.getgid()==1500 and os.getgroups()==[]
status={}
for line in open('/proc/self/status'):
    k,_,v=line.partition(':');status[k]=v.strip()
assert all(int(status[k],16)==0 for k in ['CapEff','CapPrm','CapInh','CapAmb','CapBnd'])
assert status['NoNewPrivs']=='1'
assert [name for _,name in socket.if_nameindex()]==['lo']
assert all(not os.path.exists(p) for p in json.loads(sys.argv[1]))
assert not os.path.exists('/dev/fuse')
assert open('/workspace/old','rb').read()==b'unchanged inherited content'
fd=os.open('/workspace/runtime5-qualified.txt',os.O_CREAT|os.O_EXCL|os.O_RDWR,0o600)
payload=b'actual namespace5 mapped writer\n'
assert os.write(fd,payload)==len(payload)
mapped=mmap.mmap(fd,len(payload));mapped[0:1]=b'A';mapped.flush();os.fsync(fd)
child=os.fork()
if child==0:
    while True:time.sleep(1)
fd2=os.open('/scratch/runtime5-ready',os.O_CREAT|os.O_EXCL|os.O_WRONLY,0o600)
os.write(fd2,b'uid1500;caps0;lo;private-absent;fuse-read;mmap;descendant');os.fsync(fd2);os.close(fd2)
while True:time.sleep(1)
"#;

fn qualified_request(input: &Input) -> (SourceIssuerFixture, ManagedSourcePrepareRequest) {
    let mut fixture = SourceIssuerFixture::new(None);
    fixture.binding.root = input.graph.root.clone();
    fixture.binding.observation.profile = input.graph.profile.clone();
    let owner = fixture.owner.as_mut().unwrap();
    let fence = ManagedLaunchFence {
        owner: fixture.fence.clone(),
        run_id: fixture.binding.attempt.run_id.clone(),
        run_epoch: fixture.binding.attempt.run_epoch,
        attempt_id: fixture.binding.attempt.execution_id.clone(),
        request_id: Uuid::new_v4().to_string(),
    };
    let namespace = crate::managed_store::namespace(
        &fixture.home,
        &fixture.repo,
        &fixture.fence.project_id,
        owner.journal_parts().0,
    )
    .unwrap();
    let store = owner
        .prepare_managed_project(&ManagedProjectPrepare {
            request_id: Uuid::new_v4().to_string(),
            namespace,
        })
        .unwrap();
    store
        .run(&fence.run_id)
        .unwrap()
        .persist_reservation(&crate::managed_launch::ManagedRunSource::Reserved {
            fence: fence.clone(),
            runnable_root: fixture.binding.root.root.clone(),
        })
        .unwrap();
    let selection = RemoteLaunchRequest {
        format_version: 1,
        request_id: fence.request_id.clone(),
        root: fixture.binding.root.clone(),
        attempt: fixture.binding.attempt.clone(),
        observation: fixture.binding.observation.clone(),
        accepted_source: input.graph.accepted_source.clone(),
        accepted_seed: input.graph.accepted_seed.clone(),
        projection_policy: input.graph.projection_policy.clone(),
    };
    let private = store
        .binding()
        .directory
        .join("preparations")
        .join(&fence.request_id);
    let endpoint_parent = fixture.directory().join("ep");
    new_private(&endpoint_parent).unwrap();
    let network = NetworkProfileV1 {
        format_version: 1,
        execution_profile: NetworkExecutionProfile::CloudUid1500LinuxV1,
        binding: NetworkBinding {
            project_id: fence.owner.project_id.clone(),
            owner: fence.owner.owner_id.clone(),
            owner_epoch: NetworkEpoch::new(fence.owner.epoch).unwrap(),
            run_id: fence.run_id.clone(),
            run_epoch: NetworkEpoch::new(fence.run_epoch).unwrap(),
            request_id: fence.request_id.clone(),
            execution_id: fence.attempt_id.clone(),
            operation_id: selection.attempt.operation_id.clone(),
            view_id: selection.attempt.view_id.clone(),
            network_id: Uuid::new_v4().to_string(),
        },
        mode: NetworkMode::OfflineV1,
        policy: None,
        policy_sha256: None,
        budgets: NetworkBudgets {
            max_connections: 0,
            max_dns_jobs: 0,
            max_request_bytes: 0,
            max_buffer_bytes: 0,
            max_bytes_per_connection: 0,
            max_total_bytes: 0,
            connect_timeout_ms: 0,
            idle_timeout_ms: 0,
            total_timeout_ms: 0,
        },
        readiness_nonce: Uuid::new_v4().to_string(),
    };
    let lease = fixture
        .owner
        .as_ref()
        .unwrap()
        .source_lease(&fence.owner)
        .unwrap();
    let context = RemoteContextInputs {
        node: pin(input.node.to_str().unwrap()),
        cli_script: pin(input.cli_script.to_str().unwrap()),
        source: remote_workspace::SourceContext {
            scope: input.graph.root.scope.clone(),
            base_url: input.base_url.clone(),
            auth_headers: input.auth_headers.clone(),
        },
        cache_root: private.join("cache"),
        registry_root: private.join("registry"),
    };
    let mut runtime = vec![ReadOnlyRuntime {
        source: "/usr".into(),
        guest_path: "/usr".into(),
    }];
    if Path::new("/etc/ld.so.cache").is_file() {
        runtime.push(ReadOnlyRuntime {
            source: "/etc/ld.so.cache".into(),
            guest_path: "/etc/ld.so.cache".into(),
        });
    }
    let inputs = ManagedSourceInputs {
        managed_store: store,
        endpoint_parent: endpoint_parent.clone(),
        context,
        network,
        runtime,
        command: vec![
            "/usr/bin/python3".into(),
            "-I".into(),
            "-c".into(),
            GUEST.into(),
            serde_json::to_string(&vec![
                private.clone(),
                endpoint_parent.clone(),
                fixture.home.clone(),
            ])
            .unwrap(),
        ],
        timeout_seconds: 180,
    };
    let request = ManagedSourcePrepareRequest::new(lease, fence, selection, inputs).unwrap();
    (fixture, request)
}

#[test]
#[ignore = "private actual HTTPS/FUSE/namespace5 fixture requires Root-created disposable UID1500 worker"]
fn actual_https_source5_owned_stop_checkpoint_join_and_settlement() {
    assert_eq!(unsafe { libc::geteuid() }, 1500);
    let input_path = std::env::var_os("RUDDER_RUNTIME5_SOURCE_QUALIFICATION_INPUT")
        .expect("explicit private input");
    let input_path = PathBuf::from(input_path);
    let metadata = fs::symlink_metadata(&input_path).unwrap();
    assert!(metadata.is_file() && !metadata.file_type().is_symlink());
    assert_eq!(metadata.mode() & 0o777, 0o600);
    assert_eq!(metadata.uid(), 1500);
    assert!(metadata.len() <= 16384);
    let input: Input = serde_json::from_slice(&fs::read(input_path).unwrap()).unwrap();
    assert_eq!(input.format_version, 1);
    assert!(input.base_url.starts_with("https://") && !input.base_url.contains('@'));
    assert!(
        input.native.is_absolute()
            && input.node.is_absolute()
            && input.cli_script.is_absolute()
            && input.proof.is_absolute()
    );
    let (mut fixture, request) = qualified_request(&input);
    let mut prepared = prepare(request, fixture.journal(), &AtomicBool::new(false)).unwrap();
    let selection = prepared.assignment().selection.clone();
    let spec = prepared.execution_spec().clone();
    let binding = prepared.binding().clone();
    let source_binding = binding
        .profile
        .as_ref()
        .unwrap()
        .partial_binding()
        .unwrap()
        .unwrap();
    let host_token = prepared.resources.as_ref().unwrap().host_token.0.clone();
    let guest_token = prepared.resources.as_ref().unwrap().guest_token.0.clone();
    let endpoint = prepared
        .resources
        .as_ref()
        .unwrap()
        .server
        .directory()
        .to_path_buf();
    let issuer = prepared.issuer().unwrap();
    let handoff = ManagedExecutionHandoff::new(&spec, binding.clone()).unwrap();
    let command = TerminalCommand::with_args(input.native.to_str().unwrap(), handoff.helper_args())
        .with_env("RUDDER_FABRIC_ATTEMPT_ID", &binding.execution_id)
        .with_env("RUDDER_EXECUTION_PROFILE", "offline-command-v1");
    let mut pane =
        TerminalPane::spawn_shell_or_command(Some(command), TerminalPaneOptions::default())
            .unwrap();
    let custody = pane.retain_managed_helper_custody().unwrap();
    let (receipt, handle) = handoff.activate(pane.child_process_id().unwrap()).unwrap();
    receipt.wait(Duration::from_secs(5)).unwrap();
    prepared.adopt();
    prepared
        .activate_helper(handle.helper_process_witness())
        .unwrap();
    let ready = spec
        .execution_root
        .join("runs")
        .join(&spec.execution_id)
        .join("scratch/runtime5-ready");
    let deadline = Instant::now() + Duration::from_secs(150);
    let mut backend = None;
    loop {
        pane.drain_output();
        let helper_exit = pane.try_wait_after_output().unwrap();
        assert!(
            helper_exit.is_none(),
            "helper exited before readiness: {helper_exit:?}; output: {}",
            pane.output_log_snapshot()
                .chars()
                .rev()
                .take(8192)
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>()
        );
        while let Some(event) = handle.try_event() {
            match event {
                ManagedExecutionEvent::BackendReady { process } => {
                    assert!(backend.is_none());
                    assert!(process.matches_attempt(
                        &binding.run_id,
                        &binding.execution_id,
                        "offline-command-v1"
                    ));
                    assert!(!process.authenticated_start_required());
                    process.check_current().unwrap();
                    backend = Some(process);
                }
                _ => panic!("unexpected event before authenticated stop"),
            }
        }
        if ready.exists() && backend.is_some() {
            assert_eq!(
                fs::read(&ready).unwrap(),
                b"uid1500;caps0;lo;private-absent;fuse-read;mmap;descendant"
            );
            backend.as_ref().unwrap().check_current().unwrap();
            break;
        }
        assert!(
            Instant::now() < deadline,
            "actual source5 readiness deadline; output: {}",
            pane.output_log_snapshot()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    // The owned source monitor must keep freshness current while the backend
    // and its descendant retain a mapped writer without issuing new IO.
    std::thread::sleep(Duration::from_secs(4));
    pane.drain_output();
    assert!(pane.try_wait_after_output().unwrap().is_none(),
        "helper exited during idle source freshness window; output: {}", pane.output_log_snapshot());
    backend.as_ref().unwrap().check_current().unwrap();
    prepared.stop_guest(fixture.journal()).unwrap();
    assert!(fixture
        .journal()
        .authorize_partial_workspace(&guest_token, &source_binding, now_ms().unwrap())
        .is_err());
    fixture
        .journal()
        .authorize_host_source(&host_token, &source_binding, now_ms().unwrap())
        .unwrap();
    let stop = StopAndCheckpoint {
        request_id: Uuid::new_v4().to_string(),
        execution_id: binding.execution_id.clone(),
        operation_id: binding.operation_id.clone(),
        view_id: binding.view_id.clone(),
        run_id: binding.run_id.clone(),
        run_epoch: binding.run_epoch,
        owner: binding.owner.clone(),
    };
    let budget = handle.stop_and_checkpoint_v4(stop.clone()).unwrap();
    let mut provisional = None;
    let mut exited = None;
    while provisional.is_none() || exited.is_none() {
        budget.remaining().unwrap();
        pane.drain_output();
        if exited.is_none() {
            exited = pane
                .successful_managed_helper_exit()
                .unwrap_or_else(|error| {
                    panic!(
                        "source5 helper exit: {error:#}; output: {}",
                        pane.output_log_snapshot()
                    )
                });
        }
        while let Some(event) = handle.try_event() {
            match event {
                ManagedExecutionEvent::SourceShutdown(value) => {
                    assert!(provisional.is_none());
                    assert_eq!(value.request(), &stop);
                    assert_eq!(value.record().format_version(), 2);
                    let Some(CheckpointRef::PartialRemote(checkpoint)) =
                        value.record().checkpoint()
                    else {
                        panic!("partial checkpoint required")
                    };
                    checkpoint.validate_data().unwrap();
                    assert_eq!(checkpoint.capture.base, input.graph.root);
                    let store = rudder_engine::store::LocalStore::open(&spec.store_root).unwrap();
                    assert_eq!(
                        store
                            .object_set_receipt(&checkpoint.evidence_retention.pin_id)
                            .unwrap()
                            .as_ref(),
                        Some(&checkpoint.evidence_retention)
                    );
                    let plan = rudder_engine::delta::read_partial_tree(
                        &store,
                        &checkpoint.capture.retention,
                    )
                    .unwrap();
                    assert_eq!(plan.base, input.graph.root);
                    assert_eq!(plan.attempt, source_binding.attempt);
                    assert!(!plan.remote_boundaries.is_empty());
                    let directories =
                        rudder_engine::directory::LocalDirectoryStore::read_only(&store);
                    let entry = rudder_engine::directory::PagedDirectory::new(&directories)
                        .lookup(&plan.directory, b"runtime5-qualified.txt")
                        .unwrap()
                        .expect("captured guest write");
                    let rudder_engine::directory::EntryNode::File {
                        object,
                        logical_size,
                        ..
                    } = entry.node
                    else {
                        panic!("guest write kind changed")
                    };
                    let expected = b"Actual namespace5 mapped writer\n";
                    assert_eq!(logical_size, expected.len() as u64);
                    assert_eq!(
                        rudder_engine::content::read_file_range(
                            &store,
                            &object,
                            0,
                            expected.len() + 1
                        )
                        .unwrap(),
                        expected
                    );
                    let retained = store
                        .object_set_refs(&checkpoint.evidence_retention)
                        .unwrap();
                    assert_eq!(retained.len(), 2);
                    for reference in retained {
                        store.read_object(&reference).unwrap();
                    }
                    provisional = Some(value);
                }
                _ => panic!("unexpected stop event"),
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let joined = ExitedSourceShutdown::new(
        provisional.take().unwrap(),
        issuer.clone(),
        custody,
        exited.take().unwrap(),
    )
    .unwrap()
    .join()
    .unwrap();
    joined.provisional().unwrap();
    let record = serde_json::to_value(joined.provisional().unwrap().record()).unwrap();
    prepared.retire_joined(&joined, fixture.journal()).unwrap();
    assert!(prepared.resources.is_none());
    assert!(!endpoint.exists());
    assert!(fixture
        .journal()
        .authorize_host_source(&host_token, &source_binding, now_ms().unwrap())
        .is_err());
    let checkpoint_publication = input.checkpoint_transfer.as_ref().map(|transfer| {
        let Some(CheckpointRef::PartialRemote(q)) = joined.provisional().unwrap().record().checkpoint() else {
            panic!("source5 checkpoint DATA required")
        };
        checkpoint_transfer::publish(transfer, &input, selection, q, &spec.store_root, &prepared.managed_run, fixture.directory())
            .unwrap_or_else(|error| panic!("actual checkpoint DATA publication failed: {error}"))
    });
    let proof = serde_json::json!({"format_version":1,"kind":"private-runtime5-source-qualification", "status":"qualified",
        "spec_version":5,"binding_version":4,"actual_https_source":true,"actual_backend_ready":true,
        "actual_uid1500_fuse_offline_namespace":true,"actual_stop_v4":true,"captured_mmap_bytes_verified":true,"actual_successful_helper_exit":true,
        "idle_source_freshness_window_ms":4000,"actual_helper_alive_after_idle_window":true,
        "actual_fresh_issuer_join":true,"actual_server_callbacks_settled":true,"record":record,
        "checkpoint_publication":checkpoint_publication,
        "accepted_jj_history":false,"model_execution":false,"default_rollout":false});
    write_new(&input.proof, &serde_json::to_vec(&proof).unwrap()).unwrap();
    drop(handle);
    drop(joined);
    drop(issuer);
}
