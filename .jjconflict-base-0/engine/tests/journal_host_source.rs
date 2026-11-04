//! Actual SQLite authorization tests; no Native transport or held OS owner is
//! constructed here. Scope/identity data never proves execution/history authority.
use rudder_engine::{
    journal::{
        host_source_scope, CapabilityGrant, CapabilityIdentity, HostSourceAuthorization, Journal,
        OwnerFence, HOST_SOURCE_PURPOSE, HOST_SOURCE_REQUIRED_OPERATION,
    },
    protocol::{ObjectKind, ObjectRef, OwnerEpoch},
    remote_cache::CacheScope,
    remote_root::{AttemptBinding, RootBinding},
    store::ImportPolicy,
    vfs::{
        exclusion_policy_digest,
        partial_rpc::{partial_scope, PartialBinding, PROTOCOL, REQUIRED_OPERATION},
        rpc::{AttemptContext, ObservationBinding},
    },
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

const EXECUTION: &str = "11111111-1111-1111-1111-111111111111";
const OTHER_EXECUTION: &str = "99999999-9999-9999-9999-999999999999";
fn binding() -> PartialBinding {
    let owner = OwnerEpoch {
        owner: "engine".into(),
        epoch: 1,
    };
    let attempt = AttemptBinding {
        project_id: "project".into(),
        run_id: "run".into(),
        run_epoch: 1,
        owner: owner.clone(),
        execution_id: EXECUTION.into(),
        operation_id: "22222222-2222-2222-2222-222222222222".into(),
        view_id: "33333333-3333-3333-3333-333333333333".into(),
    };
    PartialBinding {
        protocol: PROTOCOL.into(),
        format_version: 1,
        launch_request_digest: "a".repeat(64),
        root: RootBinding {
            scope: CacheScope {
                provider_id: "source-fixture".into(),
                tenant_id: "tenant-fixture".into(),
            },
            root_id: "base-root".into(),
            root: ObjectRef::from_bytes(ObjectKind::RunnableRoot, b"fixture runnable"),
        },
        observation: ObservationBinding {
            format_version: 1,
            attempt: AttemptContext {
                execution_id: attempt.execution_id.clone(),
                operation_id: attempt.operation_id.clone(),
            },
            view_id: attempt.view_id.clone(),
            owner,
            namespace: "44444444-4444-4444-4444-444444444444".into(),
            profile: ObjectRef::from_bytes(ObjectKind::Blob, b"fixture profile"),
            exclusion_policy_digest: exclusion_policy_digest(&ImportPolicy::default()).unwrap(),
        },
        attempt,
    }
}

struct Fixture {
    _root: tempfile::TempDir,
    path: PathBuf,
    journal: Journal,
    owner: OwnerFence,
    binding: PartialBinding,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("journal.sqlite");
        let mut journal = Journal::open(&path).unwrap();
        let owner = journal
            .create_project("project", "engine", "base", 0)
            .unwrap();
        journal.register_run(&owner, "run", 0).unwrap();
        journal.register_run(&owner, "other", 0).unwrap();
        journal
            .observe_run_attempt(&owner, "run", EXECUTION, 0)
            .unwrap();
        Self {
            _root: root,
            path,
            journal,
            owner,
            binding: binding(),
        }
    }
    fn grant(&self, operation: &str, scope: String) -> CapabilityGrant {
        CapabilityGrant {
            principal: "host-source:test".into(),
            run_id: self.binding.attempt.run_id.clone(),
            run_epoch: self.binding.attempt.run_epoch,
            operations: BTreeSet::from([operation.into()]),
            recipients: BTreeMap::new(),
            scopes: BTreeSet::from([scope]),
            topics: BTreeSet::new(),
            expires_at_ms: None,
        }
    }
    fn issue(&mut self, operation: &str, scope: String) -> String {
        let grant = self.grant(operation, scope);
        self.journal
            .issue_capability(&self.owner, &grant, 0)
            .unwrap()
    }
    fn host(&mut self) -> String {
        self.issue(
            HOST_SOURCE_REQUIRED_OPERATION,
            host_source_scope(&self.binding).unwrap(),
        )
    }
}

#[test]
fn host_scope_has_independent_domain_and_fixed_canonical_golden() {
    let b = binding();
    assert_eq!(HOST_SOURCE_PURPOSE, "remote_source_only_v1");
    assert_eq!(HOST_SOURCE_REQUIRED_OPERATION, "source:host:authorize");
    assert_eq!(
        host_source_scope(&b).unwrap(),
        "host-source:81362f1d2ad9e46167dfe5c1344ca61c7c56164c422767da773300eadced046b"
    );
    assert_ne!(host_source_scope(&b).unwrap(), partial_scope(&b).unwrap());
    assert_ne!(
        host_source_scope(&b).unwrap(),
        rudder_engine::execution::workspace_scope(&b.observation).unwrap()
    );
    assert_ne!(
        host_source_scope(&b)
            .unwrap()
            .strip_prefix("host-source:")
            .unwrap(),
        b.digest().unwrap()
    );
}

#[test]
fn operation_and_scope_are_independent_and_guest_families_cannot_cross() {
    let mut f = Fixture::new();
    let host_scope = host_source_scope(&f.binding).unwrap();
    let partial = partial_scope(&f.binding).unwrap();
    let complete = rudder_engine::execution::workspace_scope(&f.binding.observation).unwrap();
    let host = f.host();
    let partial_token = f.issue(REQUIRED_OPERATION, partial.clone());
    let complete_token = f.issue("workspace:observe", complete.clone());
    f.journal
        .authorize_host_source(&host, &f.binding, 1)
        .unwrap();
    f.journal
        .authorize_partial_workspace(&partial_token, &f.binding, 1)
        .unwrap();
    f.journal
        .authorize_workspace(&complete_token, &f.binding.observation, 1)
        .unwrap();
    assert!(f
        .journal
        .authorize_partial_workspace(&host, &f.binding, 1)
        .is_err());
    assert!(f
        .journal
        .authorize_workspace(&host, &f.binding.observation, 1)
        .is_err());
    for token in [&partial_token, &complete_token] {
        assert!(f
            .journal
            .authorize_host_source(token, &f.binding, 1)
            .is_err());
    }
    for (operation, scope) in [
        (REQUIRED_OPERATION, host_scope.clone()),
        ("workspace:observe", host_scope.clone()),
        (HOST_SOURCE_REQUIRED_OPERATION, partial),
        (HOST_SOURCE_REQUIRED_OPERATION, complete),
    ] {
        let token = f.issue(operation, scope);
        assert!(f
            .journal
            .authorize_host_source(&token, &f.binding, 1)
            .is_err());
    }
}

#[test]
fn guest_token_rotation_does_not_revoke_host_source_and_host_revoke_does_not_mint_guest_rights() {
    let mut f = Fixture::new();
    let host = f.host();
    let guest = f.issue(REQUIRED_OPERATION, partial_scope(&f.binding).unwrap());
    f.journal.revoke_capability(&f.owner, &guest, 1).unwrap();
    let replacement_guest = f.issue(REQUIRED_OPERATION, partial_scope(&f.binding).unwrap());
    f.journal
        .authorize_host_source(&host, &f.binding, 2)
        .unwrap();
    f.journal
        .authorize_partial_workspace(&replacement_guest, &f.binding, 2)
        .unwrap();
    f.journal.revoke_capability(&f.owner, &host, 3).unwrap();
    assert!(f
        .journal
        .authorize_host_source(&host, &f.binding, 4)
        .is_err());
    f.journal
        .authorize_partial_workspace(&replacement_guest, &f.binding, 4)
        .unwrap();
    assert!(f
        .journal
        .authorize_host_source(&replacement_guest, &f.binding, 4)
        .is_err());
}

#[test]
fn complete_binding_is_committed_including_scope_root_profile_and_launch() {
    let mut f = Fixture::new();
    let token = f.host();
    for change in 0..16 {
        let mut offered = f.binding.clone();
        match change {
            0 => offered.launch_request_digest = "b".repeat(64),
            1 => offered.root.scope.provider_id = "different-provider".into(),
            2 => offered.root.scope.tenant_id = "different-tenant".into(),
            3 => offered.root.root_id = "different-root-id".into(),
            4 => {
                offered.root.root =
                    ObjectRef::from_bytes(ObjectKind::RunnableRoot, b"different root")
            }
            5 => offered.observation.namespace = "55555555-5555-5555-5555-555555555555".into(),
            6 => {
                offered.observation.profile =
                    ObjectRef::from_bytes(ObjectKind::Blob, b"different profile")
            }
            7 => {
                offered.attempt.operation_id = OTHER_EXECUTION.into();
                offered.observation.attempt.operation_id = OTHER_EXECUTION.into();
            }
            8 => {
                offered.attempt.view_id = OTHER_EXECUTION.into();
                offered.observation.view_id = OTHER_EXECUTION.into();
            }
            9 => {
                offered.attempt.owner.owner = "different-owner".into();
                offered.observation.owner = offered.attempt.owner.clone();
            }
            10 => {
                offered.attempt.owner.epoch += 1;
                offered.observation.owner = offered.attempt.owner.clone();
            }
            11 => offered.attempt.run_id = "other".into(),
            12 => offered.attempt.run_epoch += 1,
            13 => offered.attempt.project_id = "foreign".into(),
            14 => {
                offered.attempt.execution_id = OTHER_EXECUTION.into();
                offered.observation.attempt.execution_id = OTHER_EXECUTION.into();
            }
            _ => offered.root.root.size_bytes += 1,
        }
        offered.validate().unwrap();
        assert_ne!(
            host_source_scope(&offered).unwrap(),
            host_source_scope(&f.binding).unwrap()
        );
        assert!(
            f.journal
                .authorize_host_source(&token, &offered, 1)
                .is_err(),
            "binding change {change}"
        );
    }
}

#[test]
fn listed_scope_cannot_override_actual_project_run_owner_or_execution() {
    let mut f = Fixture::new();
    for change in 0..6 {
        let mut offered = f.binding.clone();
        match change {
            0 => offered.attempt.project_id = "foreign".into(),
            1 => offered.attempt.run_id = "other".into(),
            2 => {
                offered.attempt.owner.owner = "different-owner".into();
                offered.observation.owner = offered.attempt.owner.clone();
            }
            3 => {
                offered.attempt.owner.epoch += 1;
                offered.observation.owner = offered.attempt.owner.clone();
            }
            4 => {
                offered.attempt.execution_id = OTHER_EXECUTION.into();
                offered.observation.attempt.execution_id = OTHER_EXECUTION.into();
            }
            _ => offered.attempt.run_epoch += 1,
        }
        let token = f.issue(
            HOST_SOURCE_REQUIRED_OPERATION,
            host_source_scope(&offered).unwrap(),
        );
        assert!(
            f.journal
                .authorize_host_source(&token, &offered, 1)
                .is_err(),
            "actual fence {change}"
        );
    }
    let scope = host_source_scope(&f.binding).unwrap();
    f.journal
        .observe_run_attempt(&f.owner, "other", EXECUTION, 0)
        .unwrap();
    let mut grant = f.grant(HOST_SOURCE_REQUIRED_OPERATION, scope.clone());
    grant.run_id = "other".into();
    let other_run = f.journal.issue_capability(&f.owner, &grant, 0).unwrap();
    assert!(f
        .journal
        .authorize_host_source(&other_run, &f.binding, 1)
        .is_err());
    let foreign = f
        .journal
        .create_project("foreign", "engine", "base", 0)
        .unwrap();
    f.journal.register_run(&foreign, "run", 0).unwrap();
    f.journal
        .observe_run_attempt(&foreign, "run", EXECUTION, 0)
        .unwrap();
    let grant = f.grant(HOST_SOURCE_REQUIRED_OPERATION, scope);
    let other_project = f.journal.issue_capability(&foreign, &grant, 0).unwrap();
    assert!(f
        .journal
        .authorize_host_source(&other_project, &f.binding, 1)
        .is_err());
}

#[test]
fn authorization_never_creates_a_missing_execution_and_replacement_fences_old_grants() {
    let mut f = Fixture::new();
    let mut offered = f.binding.clone();
    offered.attempt.run_id = "other".into();
    let mut grant = f.grant(
        HOST_SOURCE_REQUIRED_OPERATION,
        host_source_scope(&offered).unwrap(),
    );
    grant.run_id = "other".into();
    let token = f.journal.issue_capability(&f.owner, &grant, 0).unwrap();
    assert!(f
        .journal
        .authorize_host_source(&token, &offered, 1)
        .is_err());
    assert!(f.journal.current_run_attempt(&f.owner, "other", 1).is_err());
    f.journal
        .observe_run_attempt(&f.owner, "other", EXECUTION, 2)
        .unwrap();
    f.journal
        .authorize_host_source(&token, &offered, 3)
        .unwrap();
    let new_epoch = f
        .journal
        .observe_run_attempt(&f.owner, "other", OTHER_EXECUTION, 4)
        .unwrap();
    assert!(f
        .journal
        .authorize_host_source(&token, &offered, 5)
        .is_err());
    offered.attempt.run_epoch = new_epoch;
    grant.run_epoch = new_epoch;
    grant.scopes = BTreeSet::from([host_source_scope(&offered).unwrap()]);
    let with_old_execution = f.journal.issue_capability(&f.owner, &grant, 6).unwrap();
    assert!(f
        .journal
        .authorize_host_source(&with_old_execution, &offered, 7)
        .is_err());
}

#[test]
fn token_expiry_revocation_and_inactive_owner_are_read_currently() {
    let mut f = Fixture::new();
    let mut grant = f.grant(
        HOST_SOURCE_REQUIRED_OPERATION,
        host_source_scope(&f.binding).unwrap(),
    );
    grant.expires_at_ms = Some(10);
    let expiring = f.journal.issue_capability(&f.owner, &grant, 0).unwrap();
    f.journal
        .authorize_host_source(&expiring, &f.binding, 9)
        .unwrap();
    assert!(f
        .journal
        .authorize_host_source(&expiring, &f.binding, 10)
        .is_err());
    let revoked = f.host();
    f.journal.revoke_capability(&f.owner, &revoked, 1).unwrap();
    assert!(f
        .journal
        .authorize_host_source(&revoked, &f.binding, 2)
        .is_err());
    assert!(f
        .journal
        .authorize_host_source(&"0".repeat(64), &f.binding, 2)
        .is_err());
    let inactive = f.host();
    // Isolate owner_active: exact capability/scope/epoch/instance stay unchanged.
    // Direct SQL is only in this Engine SQLite fixture, never a Native adapter.
    let db = rusqlite::Connection::open(&f.path).unwrap();
    db.execute(
        "UPDATE projects SET owner_active=0 WHERE project_id='project'",
        [],
    )
    .unwrap();
    assert!(f
        .journal
        .authorize_host_source(&inactive, &f.binding, 2)
        .is_err());
}

#[test]
fn authorization_snapshot_preserves_one_millisecond_expiry_and_denies_at_boundary() {
    let mut f = Fixture::new();
    let mut grant = f.grant(
        HOST_SOURCE_REQUIRED_OPERATION,
        host_source_scope(&f.binding).unwrap(),
    );
    grant.expires_at_ms = Some(10);
    let token = f.journal.issue_capability(&f.owner, &grant, 0).unwrap();
    let snapshot = f
        .journal
        .authorize_host_source(&token, &f.binding, 9)
        .unwrap();
    assert_eq!(
        snapshot,
        HostSourceAuthorization {
            identity: CapabilityIdentity {
                project_id: "project".into(),
                principal: "host-source:test".into(),
                run_id: "run".into(),
                run_epoch: 1,
                owner_epoch: 1,
            },
            validated_at_ms: 9,
            expires_at_ms: Some(10),
        }
    );
    assert!(f
        .journal
        .authorize_host_source(&token, &f.binding, 10)
        .is_err());
    // A retained result reports the original validation/expiry, never a new
    // three-second window after the capability has expired.
    assert_eq!(snapshot.validated_at_ms, 9);
    assert_eq!(snapshot.expires_at_ms, Some(10));
}

#[test]
fn no_expiry_result_reports_query_time_and_rechecks_revocation() {
    let mut f = Fixture::new();
    let token = f.host();
    let snapshot = f
        .journal
        .authorize_host_source(&token, &f.binding, 9)
        .unwrap();
    assert_eq!(snapshot.validated_at_ms, 9);
    assert_eq!(snapshot.expires_at_ms, None);
    f.journal.revoke_capability(&f.owner, &token, 10).unwrap();
    assert!(f
        .journal
        .authorize_host_source(&token, &f.binding, 11)
        .is_err());
    // No token-expiry bound does not make the saved DATA result live authority.
    assert_eq!(snapshot.validated_at_ms, 9);
    assert_eq!(snapshot.expires_at_ms, None);
}

#[test]
fn owner_transfer_and_run_epoch_advance_invalidate_previously_issued_host_tokens() {
    let mut f = Fixture::new();
    let token = f.host();
    f.journal.advance_run_epoch(&f.owner, "run", 1, 1).unwrap();
    assert!(f
        .journal
        .authorize_host_source(&token, &f.binding, 2)
        .is_err());
    f.binding.attempt.run_epoch = 2;
    let token = f.host();
    f.journal
        .authorize_host_source(&token, &f.binding, 2)
        .unwrap();
    f.journal
        .transfer_owner(&f.owner, "replacement-owner", 3)
        .unwrap();
    assert!(f
        .journal
        .authorize_host_source(&token, &f.binding, 4)
        .is_err());
    let mut released = Fixture::new();
    let token = released.host();
    released.journal.release_owner(&released.owner, 1).unwrap();
    assert!(released
        .journal
        .authorize_host_source(&token, &released.binding, 2)
        .is_err());
}

#[test]
fn current_identity_preserves_sql_epochs_above_javascript_exact_range() {
    let mut f = Fixture::new();
    let above_js = 9_007_199_254_740_993u64;
    let db = rusqlite::Connection::open(&f.path).unwrap();
    db.execute(
        "UPDATE projects SET owner_epoch=?1 WHERE project_id='project'",
        [above_js as i64],
    )
    .unwrap();
    db.execute(
        "UPDATE runs SET epoch=?1 WHERE project_id='project' AND run_id='run'",
        [(above_js + 1) as i64],
    )
    .unwrap();
    f.owner.epoch = above_js;
    f.binding.attempt.owner.epoch = above_js;
    f.binding.observation.owner.epoch = above_js;
    f.binding.attempt.run_epoch = above_js + 1;
    let token = f.host();
    let authorization = f
        .journal
        .authorize_host_source(&token, &f.binding, 1)
        .unwrap();
    assert_eq!(authorization.identity.owner_epoch, above_js);
    assert_eq!(authorization.identity.run_epoch, above_js + 1);
    let mut changed = f.binding.clone();
    changed.attempt.owner.epoch += 1;
    changed.observation.owner.epoch += 1;
    assert_ne!(
        host_source_scope(&changed).unwrap(),
        host_source_scope(&f.binding).unwrap()
    );
    assert!(f
        .journal
        .authorize_host_source(&token, &changed, 1)
        .is_err());
    changed = f.binding.clone();
    changed.attempt.run_epoch = u64::MAX;
    let token = f.issue(
        HOST_SOURCE_REQUIRED_OPERATION,
        host_source_scope(&changed).unwrap(),
    );
    assert!(f
        .journal
        .authorize_host_source(&token, &changed, 1)
        .is_err());
}

#[test]
fn repeated_authorization_changes_no_schema_events_capabilities_or_authority_state() {
    let mut f = Fixture::new();
    let token = f.host();
    let observer = rusqlite::Connection::open(&f.path).unwrap();
    let version: u32 = observer
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 4);
    let data_version: i64 = observer
        .pragma_query_value(None, "data_version", |row| row.get(0))
        .unwrap();
    let before = f.journal.project_state("project").unwrap();
    let events = f.journal.events("project", 0, 1000).unwrap();
    let count: i64 = observer
        .query_row("SELECT count(*) FROM capabilities", [], |row| row.get(0))
        .unwrap();
    for now in [1, 2, 3] {
        f.journal
            .authorize_host_source(&token, &f.binding, now)
            .unwrap();
    }
    assert_eq!(f.journal.project_state("project").unwrap(), before);
    assert_eq!(f.journal.events("project", 0, 1000).unwrap(), events);
    let after_version: u32 = observer
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    let after_data: i64 = observer
        .pragma_query_value(None, "data_version", |row| row.get(0))
        .unwrap();
    let after_count: i64 = observer
        .query_row("SELECT count(*) FROM capabilities", [], |row| row.get(0))
        .unwrap();
    assert_eq!(after_version, version);
    assert_eq!(after_data, data_version);
    assert_eq!(after_count, count);
}

#[test]
fn malformed_or_wrong_protocol_binding_cannot_become_host_scope_or_authorization() {
    let mut f = Fixture::new();
    let token = f.host();
    for change in 0..5 {
        let mut offered = f.binding.clone();
        match change {
            0 => offered.protocol = "complete-observation".into(),
            1 => offered.format_version = 2,
            2 => offered.launch_request_digest = "invalid".into(),
            3 => offered.root.root.kind = ObjectKind::ActionResult,
            _ => offered.observation.exclusion_policy_digest = "b".repeat(64),
        }
        assert!(host_source_scope(&offered).is_err());
        assert!(f
            .journal
            .authorize_host_source(&token, &offered, 1)
            .is_err());
    }
}
