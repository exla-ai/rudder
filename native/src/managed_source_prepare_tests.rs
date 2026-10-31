//! Preparation and authenticated helper startup. Synthetic peer adapters below
//! do not qualify actual helper custody; no test qualifies runtime readiness,
//! namespaces, a Spec5 launch, shutdown finalization, or accepted jj history.
use super::*;
use crate::{
    source_authorizer::{HostSourceIssuerPeer, SourceAuthorizerClient},
    source_issuer::SourceIssuerFixture,
};
use rudder_engine::{
    execution::network::{
        NetworkBinding, NetworkBudgets, NetworkEpoch, NetworkExecutionProfile, NetworkMode,
    },
    journal::ManagedProjectPrepare,
    protocol::{ObjectKind, ObjectRef},
};
use std::os::unix::fs::PermissionsExt;
use uuid::Uuid;

fn pin(path: &str) -> remote_workspace::RuntimeFile {
    let bytes = fs::read(path).unwrap();
    remote_workspace::RuntimeFile {
        path: path.into(),
        sha256: super::super::hex_digest(Sha256::digest(&bytes)),
        size_bytes: bytes.len() as u64,
    }
}
pub(crate) fn request() -> (SourceIssuerFixture, ManagedSourcePrepareRequest) {
    let mut fixture = SourceIssuerFixture::new(None);
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
        .persist_reservation(&super::super::ManagedRunSource::Reserved {
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
        accepted_source: ObjectRef::from_bytes(ObjectKind::SourceRoot, b"expected source only"),
        accepted_seed: ObjectRef::from_bytes(ObjectKind::Directory, b"expected seed only"),
        projection_policy: ObjectRef::from_bytes(ObjectKind::Blob, b"expected projection policy"),
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
        node: pin("/usr/bin/true"),
        cli_script: pin("/etc/debian_version"),
        source: remote_workspace::SourceContext {
            scope: selection.root.scope.clone(),
            base_url: "https://source.invalid".into(),
            auth_headers: BTreeMap::from([(
                "authorization".into(),
                "Bearer fixture-fabric-secret".into(),
            )]),
        },
        cache_root: private.join("cache"),
        registry_root: private.join("registry"),
    };
    let inputs = ManagedSourceInputs {
        managed_store: store,
        endpoint_parent,
        context,
        network,
        runtime: vec![ReadOnlyRuntime {
            source: "/usr".into(),
            guest_path: "/usr".into(),
        }],
        command: vec!["/usr/bin/true".into()],
        timeout_seconds: 60,
    };
    let request = ManagedSourcePrepareRequest::new(lease, fence, selection, inputs).unwrap();
    (fixture, request)
}

/// Startup-only fixture cleanup after the test actually reaps its successful
/// child and drops control. This is no runtime/checkpoint settlement witness.
pub(crate) fn retire_reaped_startup_fixture(
    prepared: &mut PreparedManagedSource, journal: &mut Journal,
) -> Result<()> {
    prepared.retire(journal, None)
}

#[test]
fn source_request_binds_actual_lease_owner_and_exact_reserved_selection() {
    let (fixture, mut req) = request();
    req.selection.attempt.run_epoch += 1;
    assert!(
        ManagedSourcePrepareRequest::new(req.lease, req.fence, req.selection, req.inputs).is_err()
    );
    assert!(fixture.owner.as_ref().unwrap().assert_current().is_ok());
}

#[test]
fn cancelled_or_stale_preparation_mints_no_endpoint() {
    for stale in [false, true] {
        let (mut fixture, req) = request();
        let endpoint = req.inputs.endpoint_parent.join(&req.fence.request_id);
        if stale {
            fixture
                .journal()
                .observe_run_attempt(
                    &req.fence.owner,
                    &req.fence.run_id,
                    &Uuid::new_v4().to_string(),
                    now_ms().unwrap(),
                )
                .unwrap();
        }
        assert!(prepare(req, fixture.journal(), &AtomicBool::new(!stale)).is_err());
        assert!(!endpoint.exists());
    }
}

#[test]
fn source_preparation_refuses_another_actual_journal_before_issuance() {
    let (_fixture, req) = request();
    let mut foreign = SourceIssuerFixture::new(None);
    let endpoint = req.inputs.endpoint_parent.join(&req.fence.request_id);
    assert!(req.lease.check_journal(foreign.journal()).is_err());
    assert!(prepare(req, foreign.journal(), &AtomicBool::new(false)).is_err());
    assert!(!endpoint.exists());
}

#[test]
fn source_preparation_issues_separate_grants_and_keeps_host_current_after_guest_stop() {
    let (mut fixture, req) = request();
    let selection = req.selection.clone();
    let mut prepared = prepare(req, fixture.journal(), &AtomicBool::new(false)).unwrap();
    assert_eq!(prepared.execution_spec().format_version, 5);
    assert_eq!(prepared.binding().format_version, 4);
    assert_eq!(prepared.assignment().format_version, 2);
    assert_eq!(prepared.assignment().selection, selection);
    prepared
        .binding()
        .validate(prepared.execution_spec())
        .unwrap();
    let context = serde_json::from_slice::<serde_json::Value>(
        &prepared.assignment().host_context.read_bytes().unwrap(),
    )
    .unwrap();
    assert_eq!(context["format_version"], 2);
    assert_eq!(prepared.assignment().host_context.format_version, 1);
    assert!(
        !String::from_utf8(canonical_json(prepared.execution_spec()).unwrap())
            .unwrap()
            .contains("fixture-fabric-secret")
    );
    let root = prepared
        .assignment()
        .host_context
        .path
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    assert!(root.join("assignment2.json").is_file());
    let binding = prepared
        .binding
        .profile
        .as_ref()
        .unwrap()
        .partial_binding()
        .unwrap()
        .unwrap();
    let resources = prepared.resources.as_ref().unwrap();
    let host = resources.host_token.0.clone();
    let guest = resources.guest_token.0.clone();
    assert_ne!(host, guest);
    assert!(fixture
        .journal()
        .authorize_host_source(&guest, &binding, now_ms().unwrap())
        .is_err());
    assert!(fixture
        .journal()
        .authorize_partial_workspace(&host, &binding, now_ms().unwrap())
        .is_err());
    assert!(fixture
        .journal()
        .authorize_partial_workspace(&guest, &binding, now_ms().unwrap())
        .is_ok());
    prepared.stop_guest(fixture.journal()).unwrap();
    assert!(fixture
        .journal()
        .authorize_partial_workspace(&guest, &binding, now_ms().unwrap())
        .is_err());
    prepared.issuer().unwrap().authorize(&binding).unwrap();
    let source = prepared
        .resources
        .as_ref()
        .unwrap()
        .server
        .directory()
        .to_path_buf();
    let workspace = &prepared
        .execution_spec()
        .workspace_observation
        .as_ref()
        .unwrap()
        .endpoint_directory;
    assert!(prepared
        .execution_spec()
        .authority_roots
        .iter()
        .all(|authority| !workspace.starts_with(authority) && !authority.starts_with(workspace)));
    assert!(prepared
        .execution_spec()
        .authority_roots
        .iter()
        .any(|authority| source.starts_with(authority)));
    let context = prepared.assignment().host_context.path.clone();
    prepared.retire_unused(fixture.journal()).unwrap();
    assert!(!source.exists());
    assert!(!context.exists());
    assert!(fixture
        .journal()
        .authorize_host_source(&host, &binding, now_ms().unwrap())
        .is_err());
}

// Client-side test adapter only. It cannot activate the production server gate.
struct SyntheticIssuerPeer;
impl HostSourceIssuerPeer for SyntheticIssuerPeer {
    fn check_current(&self) -> Result<()> {
        Ok(())
    }
    fn check_peer(&self, _: &UnixStream) -> Result<()> {
        Ok(())
    }
}
#[test]
fn host_endpoint_refuses_requests_without_actual_helper_activation() {
    let (mut fixture, req) = request();
    let mut prepared = prepare(req, fixture.journal(), &AtomicBool::new(false)).unwrap();
    let binding = prepared
        .binding
        .profile
        .as_ref()
        .unwrap()
        .partial_binding()
        .unwrap()
        .unwrap();
    let directory = prepared
        .resources
        .as_ref()
        .unwrap()
        .server
        .directory()
        .to_path_buf();
    let client = || {
        SourceAuthorizerClient::connect(&directory, binding.clone(), Arc::new(SyntheticIssuerPeer))
    };
    assert!(client().is_err());
    prepared
        .resources
        .as_ref()
        .unwrap()
        .peer
        .0
        .check_pending()
        .unwrap();
    prepared.stop_guest(fixture.journal()).unwrap();
    assert!(client().is_err());
    prepared.issuer().unwrap().authorize(&binding).unwrap();
    prepared.retire_unused(fixture.journal()).unwrap();
}

#[test]
fn pending_activation_waits_for_install_and_failure_or_expiry_performs_no_callback() {
    // This tests only the slot scheduling primitive. A marker cannot construct
    // HelperProcessWitness or authenticate any real process/socket.
    let slot = Arc::new(ActivationSlot::<u8>::new());
    let worker_slot = slot.clone();
    let (arrived, arrival) = std::sync::mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        worker_slot
            .wait_pending(Duration::from_secs(1), || arrived.send(()).unwrap())
            .unwrap()
    });
    arrival.recv_timeout(Duration::from_secs(1)).unwrap();
    slot.install(Arc::new(7)).unwrap();
    assert_eq!(*worker.join().unwrap(), 7);

    let slot = ActivationSlot::<u8>::new();
    assert!(slot.wait(Duration::ZERO).is_err());
    slot.check_pending().unwrap(); // a request timeout cannot revoke activation
    slot.install(Arc::new(8)).unwrap();
    assert_eq!(*slot.wait(Duration::ZERO).unwrap(), 8);

    for failed in [true, false] {
        let mut slot = ActivationSlot::<u8>::new();
        if failed {
            slot.fail();
        } else {
            slot.deadline = Instant::now();
        }
        let queried = AtomicBool::new(false);
        assert!(slot
            .wait(Duration::from_millis(10))
            .map(|_| queried.store(true, Ordering::Release))
            .is_err());
        assert!(!queried.load(Ordering::Acquire));
        assert!(slot.check_pending().is_err());
        assert!(slot.install(Arc::new(9)).is_err());
    }
}

#[test]
fn charged_server_worker_waits_for_activation_before_actual_issuer_query() {
    // Synthetic transport admission tests the startup race only. It cannot
    // qualify the production HelperProcessWitness or helper kernel custody.
    struct TestGate {
        slot: Arc<ActivationSlot<u8>>,
        arrived: std::sync::mpsc::SyncSender<()>,
    }
    impl HostSourceRequestPeer for TestGate {
        fn check_peer(&self, _: &UnixStream) -> Result<()> {
            self.slot.wait_pending(Duration::from_secs(1), || {
                let _ = self.arrived.try_send(());
            })?;
            Ok(())
        }
    }
    struct CountQueries {
        issuer: Arc<HostSourceIssuer>,
        count: std::sync::atomic::AtomicUsize,
    }
    impl HostSourceAuthority for CountQueries {
        fn authorize(
            &self,
            binding: &PartialBinding,
        ) -> Result<rudder_engine::journal::HostSourceAuthorization> {
            self.count.fetch_add(1, Ordering::AcqRel);
            self.issuer.authorize(binding)
        }
    }
    for outcome in [0, 1, 2] {
        // install after request, failure, expiry
        let fixture = SourceIssuerFixture::new(None);
        let mut pending = ActivationSlot::new();
        if outcome == 2 {
            pending.deadline = Instant::now();
        }
        let slot = Arc::new(pending);
        if outcome == 1 {
            slot.fail();
        }
        let (arrived, arrival) = std::sync::mpsc::sync_channel(1);
        let authority = Arc::new(CountQueries {
            issuer: fixture.issuer().clone(),
            count: std::sync::atomic::AtomicUsize::new(0),
        });
        let mut server = SourceAuthorizerServer::bind_with_request_peer(
            &fixture.directory().join("gate"),
            fixture.binding.clone(),
            authority.clone(),
            Limits::default(),
            Arc::new(TestGate {
                slot: slot.clone(),
                arrived,
            }),
        )
        .unwrap();
        let directory = server.directory().to_path_buf();
        let binding = fixture.binding.clone();
        // connect performs its first authenticated query, so it must run in
        // the waiting client worker before activation is installed.
        let worker = std::thread::spawn(move || {
            SourceAuthorizerClient::connect(&directory, binding, Arc::new(SyntheticIssuerPeer))
        });
        if outcome == 0 {
            arrival.recv_timeout(Duration::from_secs(1)).unwrap();
            assert_eq!(authority.count.load(Ordering::Acquire), 0);
            slot.install(Arc::new(7)).unwrap();
            worker.join().unwrap().unwrap();
            assert!(authority.count.load(Ordering::Acquire) > 0);
        } else {
            assert!(worker.join().unwrap().is_err());
            assert_eq!(authority.count.load(Ordering::Acquire), 0);
        }
        server.await_closed(Duration::from_secs(2)).unwrap();
    }
}

#[test]
fn guest_stop_refuses_a_foreign_writer_and_preserves_original_grants() {
    let (mut fixture, req) = request();
    let mut prepared = prepare(req, fixture.journal(), &AtomicBool::new(false)).unwrap();
    let mut foreign = SourceIssuerFixture::new(None);
    assert!(prepared.stop_guest(foreign.journal()).is_err());
    let binding = prepared
        .binding
        .profile
        .as_ref()
        .unwrap()
        .partial_binding()
        .unwrap()
        .unwrap();
    let token = prepared.resources.as_ref().unwrap().guest_token.0.clone();
    fixture
        .journal()
        .authorize_partial_workspace(&token, &binding, now_ms().unwrap())
        .unwrap();
    prepared.retire_unused(fixture.journal()).unwrap();
}

#[test]
fn protected_context_custody_refuses_identical_replacement_and_changed_bytes() {
    let (mut fixture, req) = request();
    let mut prepared = prepare(req, fixture.journal(), &AtomicBool::new(false)).unwrap();
    let path = prepared.assignment().host_context.path.clone();
    let bytes = fs::read(&path).unwrap();
    let saved = path.with_extension("saved");
    fs::rename(&path, &saved).unwrap();
    write_new(&path, &bytes).unwrap();
    assert!(prepared
        .resources
        .as_mut()
        .unwrap()
        .context
        .check_current()
        .is_err());
    fs::remove_file(&path).unwrap();
    fs::rename(&saved, &path).unwrap();
    let mut changed = bytes.clone();
    changed[0] ^= 1;
    fs::write(&path, changed).unwrap();
    assert!(prepared
        .resources
        .as_mut()
        .unwrap()
        .context
        .check_current()
        .is_err());
    fs::write(&path, bytes).unwrap();
    prepared
        .resources
        .as_mut()
        .unwrap()
        .context
        .check_current()
        .unwrap();
    prepared.retire_unused(fixture.journal()).unwrap();
}

#[test]
fn adopted_source_cannot_retire_without_join_and_cleanup_refuses_replacement_context() {
    let (mut fixture, req) = request();
    let mut prepared = prepare(req, fixture.journal(), &AtomicBool::new(false)).unwrap();
    prepared.adopt();
    assert!(prepared.retire_unused(fixture.journal()).is_err());
    // No actual helper was launched. Undo only this test's local adoption marker
    // so it can test unused cleanup; this grants no joined shutdown proof.
    prepared.adopted = false;
    let context = prepared
        .assignment()
        .host_context
        .path
        .parent()
        .unwrap()
        .to_path_buf();
    let saved = context.with_extension("saved");
    fs::rename(&context, &saved).unwrap();
    new_private(&context).unwrap();
    fs::write(context.join("replacement"), b"preserve").unwrap();
    fs::set_permissions(
        context.join("replacement"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    assert!(prepared.retire_unused(fixture.journal()).is_err());
    assert_eq!(fs::read(context.join("replacement")).unwrap(), b"preserve");
    fs::remove_file(context.join("replacement")).unwrap();
    fs::remove_dir(&context).unwrap();
    fs::rename(saved, context).unwrap();
    prepared.retire_unused(fixture.journal()).unwrap();
}

#[test]
fn source_grant_bounds_cover_original_startup_execution_and_host_finalization() {
    let bounds = SourceGrantBounds::new(1_000, 60).unwrap();
    assert_eq!(bounds.guest_expiry_ms, 96_000);
    assert_eq!(bounds.host_expiry_ms, 126_000);
    // Almost twenty seconds preparing plus almost ten activating still leaves
    // the full command, thirty-second finalization and fixed safety margin.
    assert!(bounds
        .check_phase(Duration::from_millis(19_999), 20_999, PREPARATION_LIMIT)
        .is_ok());
    assert!(bounds
        .check_phase(Duration::from_millis(29_998), 30_998, STARTUP_LIMIT)
        .is_ok());
    assert!(bounds
        .check_phase(PREPARATION_LIMIT, 21_000, PREPARATION_LIMIT)
        .is_err());
    assert!(bounds
        .check_phase(STARTUP_LIMIT, 31_000, STARTUP_LIMIT)
        .is_err());
    for (start, timeout) in [
        (i64::MAX, 1),
        (i64::MAX - 95_000, 60),
        (-1, 60),
        (0, 0),
        (0, 86_401),
        (0, u64::MAX),
    ] {
        assert!(SourceGrantBounds::new(start, timeout).is_err());
    }
}

#[test]
fn sealed_source_profile_carries_the_original_preparation_clock_through_child_admission() {
    use rudder_engine::execution::startup_budget::{StartupBudget, StartupConstraint};
    let (mut fixture, req) = request();
    let mut prepared = prepare(req, fixture.journal(), &AtomicBool::new(false)).unwrap();
    let original = prepared.resources.as_ref().unwrap().peer.0.startup.as_ref().unwrap();
    let expected = StartupConstraint::from_original(original.budget.constraint(),
        original.bounds.start_ms).unwrap();
    let sealed = prepared.execution_spec().profile.as_ref().unwrap().startup.as_ref().unwrap();
    assert_eq!(sealed, &expected);
    assert_eq!(prepared.binding().profile.as_ref().unwrap().startup.as_ref(), Some(sealed));
    let wire = canonical_json(prepared.execution_spec()).unwrap();
    let child: ExecutionSpec = serde_json::from_slice(&wire).unwrap();
    assert_eq!(child.profile.as_ref().unwrap().startup.as_ref(), Some(sealed));
    let parent_remaining = original.budget.remaining().unwrap();
    let child_remaining = StartupBudget::from_constraint(sealed).unwrap().remaining().unwrap();
    assert!(child_remaining <= parent_remaining, "sealed child renewed startup time");
    prepared.retire_unused(fixture.journal()).unwrap();
}

#[test]
fn source_near_expiry_and_clock_changes_refuse_admission_without_renewal() {
    let bounds = SourceGrantBounds::new(1_000, 60).unwrap();
    // Wall time and suspend-aware time independently fence startup. A slow or
    // regressed clock cannot admit a command after the original TTL window.
    assert!(bounds
        .check_phase(Duration::ZERO, 31_000, STARTUP_LIMIT)
        .is_err());
    assert!(bounds
        .check_phase(Duration::ZERO, 999, STARTUP_LIMIT)
        .is_err());
    assert!(bounds
        .check_phase(Duration::from_secs(30), 1_000, STARTUP_LIMIT)
        .is_err());
    let short_host = SourceGrantBounds {
        host_expiry_ms: 125_998,
        ..SourceGrantBounds::new(1_000, 60).unwrap()
    };
    assert!(short_host
        .check_phase(Duration::from_millis(29_999), 30_999, STARTUP_LIMIT)
        .is_err());
    let short_guest = SourceGrantBounds {
        guest_expiry_ms: 95_998,
        ..SourceGrantBounds::new(1_000, 60).unwrap()
    };
    assert!(short_guest
        .check_phase(Duration::from_millis(29_999), 30_999, STARTUP_LIMIT)
        .is_err());
    assert_eq!(bounds.host_expiry_ms, 126_000);
    assert_eq!(bounds.guest_expiry_ms, 96_000);
}

#[test]
fn source_activation_wait_during_preparation_does_not_consume_its_ten_second_phase() {
    let startup = SourceStartupBudget::begin(60).unwrap();
    let slot = Arc::new(ActivationSlot::<u8>::with_startup(startup.clone()).unwrap());
    assert!(startup.activation_remaining().is_err());
    assert!(slot.install(Arc::new(1)).is_err());
    slot.check_pending().unwrap();
    let worker_slot = slot.clone();
    let (arrived, arrival) = std::sync::mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        worker_slot
            .wait_pending(Duration::from_secs(1), || arrived.send(()).unwrap())
            .unwrap()
    });
    arrival.recv_timeout(Duration::from_secs(1)).unwrap();
    startup.finish_preparation().unwrap();
    assert!(startup.finish_preparation().is_err());
    assert!(startup.activation_remaining().unwrap() <= ACTIVATION_LIMIT);
    slot.install(Arc::new(7)).unwrap();
    assert_eq!(*worker.join().unwrap(), 7);
}

#[test]
fn source_original_budget_expiry_absorbs_clock_recovery_and_prevents_callbacks() {
    let mut startup = SourceStartupBudget::begin(60).unwrap();
    let start = now_ms().unwrap() - 30_000;
    Arc::get_mut(&mut startup).unwrap().bounds = SourceGrantBounds::new(start, 60).unwrap();
    let slot = ActivationSlot::<u8>::with_startup(startup.clone()).unwrap();
    let queried = AtomicBool::new(false);
    assert!(slot
        .wait_pending(Duration::from_secs(1), || queried
            .store(true, Ordering::Release))
        .is_err());
    assert!(!queried.load(Ordering::Acquire));
    assert!(slot.install(Arc::new(1)).is_err());
    drop(slot);
    // Restoring a plausible wall time cannot revive the absorbing original
    // startup clock after its phase check failed.
    Arc::get_mut(&mut startup).unwrap().bounds =
        SourceGrantBounds::new(now_ms().unwrap(), 60).unwrap();
    assert!(startup.check_preparation().is_err());
}

#[test]
fn source_activation_phase_never_restarts_the_original_budget() {
    let bounds = SourceGrantBounds::new(1_000, 60).unwrap();
    for (prepared, end) in [(2_000, 12_000), (19_999, 29_999), (25_000, 30_000)] {
        let deadline = SourceGrantBounds::activation_end(Duration::from_millis(prepared)).unwrap();
        assert_eq!(deadline, Duration::from_millis(end));
        assert!(bounds
            .check_phase(
                Duration::from_millis(end - 1),
                1_000 + end as i64 - 1,
                deadline
            )
            .is_ok());
        assert!(bounds
            .check_phase(Duration::from_millis(end), 1_000 + end as i64, deadline)
            .is_err());
    }
    assert!(SourceGrantBounds::activation_end(Duration::MAX).is_err());
}

const STARTUP_CHILD_INPUT: &str = "RUDDER_SOURCE_PREPARE_STARTUP_INPUT";
const STARTUP_CHILD: &str =
    "managed_launch::source_prepare::tests::private_source_prepare_startup_child";

// Routing expectations only: neither capability nor endpoint key is serialized.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StartupChildInput {
    handoff: String,
    parent: u32,
    endpoint: PathBuf,
    notifier: PathBuf,
    binding: PartialBinding,
}

struct StartupChild(Option<std::process::Child>);
impl StartupChild {
    fn reap(mut self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.0.as_mut().unwrap().try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "startup child exit deadline");
            std::thread::sleep(Duration::from_millis(2));
        }
        let output = self.0.take().unwrap().wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
impl Drop for StartupChild {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[test]
fn actual_prepared_source_startup_authenticates_helper_and_retires_custody() {
    use rudder_engine::execution::handoff::ManagedExecutionHandoff;
    use std::{
        os::unix::net::UnixListener,
        process::{Command, Stdio},
    };

    // Both cases use the actual lease, Journal, store, fixed grants and sealed
    // three-descriptor handoff. This is startup composition, not run_helper5.
    for retire_handle in [false, true] {
        let (mut fixture, req) = request();
        let mut prepared = prepare(req, fixture.journal(), &AtomicBool::new(false)).unwrap();
        let binding = prepared
            .binding()
            .profile
            .as_ref()
            .unwrap()
            .partial_binding()
            .unwrap()
            .unwrap();
        let resources = prepared.resources.as_ref().unwrap();
        let host_token = resources.host_token.0.clone();
        let guest_token = resources.guest_token.0.clone();
        let handoff =
            ManagedExecutionHandoff::new(prepared.execution_spec(), prepared.binding().clone())
                .unwrap();
        let notifier = fixture.directory().join("startup.sock");
        let listener = UnixListener::bind(&notifier).unwrap();
        listener.set_nonblocking(true).unwrap();
        let input = StartupChildInput {
            handoff: handoff.helper_args()[4].clone(),
            parent: std::process::id(),
            endpoint: resources.server.directory().to_path_buf(),
            notifier,
            binding: binding.clone(),
        };
        let path = fixture.directory().join("startup-input.json");
        write_new(&path, &serde_json::to_vec(&input).unwrap()).unwrap();
        let child = StartupChild(Some(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--ignored",
                    "--exact",
                    STARTUP_CHILD,
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env_clear()
                .env(STARTUP_CHILD_INPUT, &path)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        ));
        // The PID comes directly from this owned Child, never saved input.
        let (receipt, handle) = handoff.activate(child.0.as_ref().unwrap().id()).unwrap();
        receipt.wait(Duration::from_secs(3)).unwrap();
        let witness = handle.helper_process_witness();
        prepared.adopt();
        prepared.activate_helper(witness.clone()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut notify = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "startup child connect deadline");
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(error) => panic!("startup accept: {error}"),
            }
        };
        notify
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        notify
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        notify.write_all(b"1").unwrap();
        let mut phase = [0];
        notify.read_exact(&mut phase).unwrap();
        assert_eq!(phase, *b"1"); // real child completed authenticated Journal query
        prepared.stop_guest(fixture.journal()).unwrap();
        assert!(fixture
            .journal()
            .authorize_partial_workspace(&guest_token, &binding, now_ms().unwrap(),)
            .is_err());
        fixture
            .journal()
            .authorize_host_source(&host_token, &binding, now_ms().unwrap())
            .unwrap();
        notify.write_all(b"2").unwrap();
        notify.read_exact(&mut phase).unwrap();
        assert_eq!(phase, *b"2"); // same authenticated host client still succeeds
        let mut handle = Some(handle);
        if retire_handle {
            drop(handle.take());
            assert!(witness.check_current().is_err());
            notify.write_all(b"3").unwrap();
            notify.read_exact(&mut phase).unwrap();
            assert_eq!(phase, *b"3"); // live child is denied after handle retirement
        } else {
            witness.check_current().unwrap();
            notify.write_all(b"4").unwrap();
        }
        child.reap();
        assert!(witness.check_current().is_err());
        drop(handle);
        // Private fixture cleanup after actual reap. This deliberately does not
        // manufacture JoinedSourceShutdown or claim a successful execution.
        prepared.retire(fixture.journal(), None).unwrap();
        assert!(fixture
            .journal()
            .authorize_host_source(&host_token, &binding, now_ms().unwrap(),)
            .is_err());
    }
}

#[test]
#[ignore = "private cleared-environment child invoked by actual preparation fixture"]
fn private_source_prepare_startup_child() {
    use rudder_engine::execution::handoff::receive_managed;
    let path = std::env::var_os(STARTUP_CHILD_INPUT).expect("private parent required");
    let input: StartupChildInput = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    let (spec, control) = receive_managed(&input.handoff, input.parent).unwrap();
    assert_eq!(spec.format_version, 5);
    assert_eq!(control.binding.format_version, 4);
    assert_eq!(
        control
            .binding
            .profile
            .as_ref()
            .unwrap()
            .partial_binding()
            .unwrap(),
        Some(input.binding.clone())
    );
    let mut notify = UnixStream::connect(&input.notifier).unwrap();
    notify
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    notify
        .set_write_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut phase = [0];
    notify.read_exact(&mut phase).unwrap();
    assert_eq!(phase, *b"1");
    let client = SourceAuthorizerClient::connect(
        &input.endpoint,
        input.binding.clone(),
        control.controller_process(),
    )
    .unwrap();
    client.authorize(&input.binding).unwrap();
    notify.write_all(b"1").unwrap();
    notify.read_exact(&mut phase).unwrap();
    assert_eq!(phase, *b"2");
    client.authorize(&input.binding).unwrap();
    notify.write_all(b"2").unwrap();
    notify.read_exact(&mut phase).unwrap();
    if phase == *b"3" {
        assert!(client.authorize(&input.binding).is_err());
        assert!(client.check_current().is_err());
        notify.write_all(b"3").unwrap();
    } else {
        assert_eq!(phase, *b"4");
    }
    drop(control);
}

#[path = "runtime5_source_qualification.rs"]
mod runtime5_source_qualification;
