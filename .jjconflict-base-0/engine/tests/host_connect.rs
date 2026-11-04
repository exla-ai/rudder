#![cfg(target_os = "linux")]
//! Exact host egress grants. No sockets, credentials or model calls.
use rudder_engine::{
    execution::network::{connected::{ConnectBinding, CONNECT_TRANSPORT_PROTOCOL},
        NetworkBinding, NetworkEpoch},
    journal::{host_connect_principal, host_connect_scope, CapabilityGrant, Journal,
        OwnerFence, HOST_CONNECT_REQUIRED_OPERATION},
};
use std::{collections::{BTreeMap, BTreeSet}, time::Duration};
use uuid::Uuid;
use std::os::unix::fs::PermissionsExt;

const ISSUED: i64 = 1_000;
const EXPIRY: i64 = 10_000;
struct Fixture {
    journal: Journal,
    owner: OwnerFence,
    binding: ConnectBinding,
    grant: CapabilityGrant,
    token: String,
    _directory: tempfile::TempDir,
}
fn fixture() -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut journal = Journal::open(directory.path().join("journal.sqlite")).unwrap();
    let owner = journal.create_project("p", "owner", "base", 0).unwrap();
    let execution_id = Uuid::new_v4().to_string();
    let epoch = journal.observe_run_attempt(&owner, "run", &execution_id, 0).unwrap();
    let binding = ConnectBinding { format_version: 1, protocol: CONNECT_TRANSPORT_PROTOCOL.into(),
        profile_sha256: "a".repeat(64), network: NetworkBinding {
            project_id: owner.project_id.clone(), owner: owner.owner_id.clone(),
            owner_epoch: NetworkEpoch::new(owner.epoch).unwrap(), run_id: "run".into(),
            run_epoch: NetworkEpoch::new(epoch).unwrap(), request_id: Uuid::new_v4().to_string(),
            execution_id, operation_id: Uuid::new_v4().to_string(),
            view_id: Uuid::new_v4().to_string(), network_id: Uuid::new_v4().to_string(),
        } };
    let grant = CapabilityGrant { principal: host_connect_principal(&binding).unwrap(),
        run_id: "run".into(), run_epoch: epoch,
        operations: [HOST_CONNECT_REQUIRED_OPERATION.into()].into(),
        recipients: BTreeMap::new(), scopes: [host_connect_scope(&binding).unwrap()].into(),
        topics: BTreeSet::new(), expires_at_ms: Some(EXPIRY) };
    let token = journal.issue_capability(&owner, &grant, ISSUED).unwrap();
    Fixture { _directory: directory, journal, owner, binding, grant, token }
}

#[test]
fn held_readonly_connection_authorizes_exact_finite_snapshot_and_sees_fresh_revocation() {
    let mut fixture = fixture();
    let held = fixture.journal.hold_database_file().unwrap();
    let read = held.open_read_only(Duration::from_millis(20)).unwrap();
    let before_sequence = fixture.journal.project_state("p").unwrap().sequence;
    let authorization = read.authorize_host_connect(&fixture.token, &fixture.binding, EXPIRY, 1_001).unwrap();
    assert_eq!(authorization.identity.project_id, "p");
    assert_eq!(authorization.identity.run_id, "run");
    assert_eq!(authorization.validated_at_ms, 1_001);
    assert_eq!(authorization.issued_at_ms, ISSUED);
    assert_eq!(authorization.expires_at_ms, EXPIRY);
    assert_eq!(fixture.journal.project_state("p").unwrap().sequence, before_sequence);
    fixture.journal.revoke_capability(&fixture.owner, &fixture.token, 1_002).unwrap();
    assert!(read.authorize_host_connect(&fixture.token, &fixture.binding, EXPIRY, 1_003).is_err());
}

#[test]
fn all_sealed_network_and_profile_fields_change_the_host_scope() {
    let fixture = fixture();
    let original = host_connect_scope(&fixture.binding).unwrap();
    for key in ["project_id", "owner", "run_id", "request_id", "execution_id", "operation_id", "view_id", "network_id"] {
        let mut wire = serde_json::to_value(&fixture.binding).unwrap();
        wire["network"][key] = Uuid::new_v4().to_string().into();
        let changed: ConnectBinding = serde_json::from_value(wire).unwrap();
        assert_ne!(original, host_connect_scope(&changed).unwrap(), "{key}");
    }
    for key in ["owner_epoch", "run_epoch", "profile_sha256"] {
        let mut changed = fixture.binding.clone();
        match key {
            "owner_epoch" => changed.network.owner_epoch = NetworkEpoch::new(2).unwrap(),
            "run_epoch" => changed.network.run_epoch = NetworkEpoch::new(2).unwrap(),
            _ => changed.profile_sha256 = "b".repeat(64),
        }
        assert_ne!(original, host_connect_scope(&changed).unwrap(), "{key}");
    }
    for key in ["protocol", "format_version", "digest_encoding"] {
        let mut changed = fixture.binding.clone();
        match key {
            "protocol" => changed.protocol = "partial-workspace".into(),
            "format_version" => changed.format_version = 2,
            _ => changed.profile_sha256 = "A".repeat(64),
        }
        assert!(host_connect_scope(&changed).is_err(), "{key}");
    }
}

#[test]
fn guest_source_mixed_nonfinite_and_wrong_expiry_grants_do_not_authorize_egress() {
    let mut fixture = fixture();
    for kind in ["guest", "source", "mixed", "wrong_scope", "wrong_principal", "unbounded", "renewed"] {
        let mut grant = fixture.grant.clone();
        match kind {
            "guest" => grant.operations = ["workspace:partial".into()].into(),
            "source" => grant.operations = ["source:host:authorize".into()].into(),
            "mixed" => { grant.operations.insert("message:send".into()); }
            "wrong_scope" => grant.scopes = ["host-connect:wrong".into()].into(),
            "wrong_principal" => grant.principal = "guest".into(),
            "unbounded" => grant.expires_at_ms = None,
            _ => grant.expires_at_ms = Some(EXPIRY + 1),
        }
        let token = fixture.journal.issue_capability(&fixture.owner, &grant, ISSUED).unwrap();
        assert!(fixture.journal.authorize_host_connect(&token, &fixture.binding, EXPIRY, ISSUED).is_err(), "{kind}");
    }
    for now in [-1, ISSUED - 1, EXPIRY, EXPIRY + 1] {
        assert!(fixture.journal.authorize_host_connect(&fixture.token, &fixture.binding, EXPIRY, now).is_err(), "{now}");
    }
    assert!(fixture.journal.authorize_host_connect(&fixture.token, &fixture.binding, EXPIRY + 1, ISSUED).is_err());
}

#[test]
fn exact_scope_alone_cannot_override_current_owner_or_execution_or_epoch() {
    for kind in ["owner", "execution", "run_epoch", "owner_epoch"] {
        let mut fixture = fixture();
        let mut binding = fixture.binding.clone();
        match kind {
            "owner" => binding.network.owner = "other-owner".into(),
            "execution" => binding.network.execution_id = Uuid::new_v4().to_string(),
            "run_epoch" => binding.network.run_epoch = NetworkEpoch::new(2).unwrap(),
            _ => binding.network.owner_epoch = NetworkEpoch::new(2).unwrap(),
        }
        let mut grant = fixture.grant.clone();
        grant.scopes = [host_connect_scope(&binding).unwrap()].into();
        let token = fixture.journal.issue_capability(&fixture.owner, &grant, ISSUED).unwrap();
        assert!(fixture.journal.authorize_host_connect(&token, &binding, EXPIRY, ISSUED).is_err(), "{kind}");
    }
    for action in ["replace_attempt", "release_owner", "transfer_owner", "run_epoch"] {
        let mut fixture = fixture();
        let held = fixture.journal.hold_database_file().unwrap();
        let read = held.open_read_only(Duration::from_millis(20)).unwrap();
        read.authorize_host_connect(&fixture.token, &fixture.binding, EXPIRY, ISSUED).unwrap();
        match action {
            "replace_attempt" => { fixture.journal.observe_run_attempt(&fixture.owner, "run", &Uuid::new_v4().to_string(), ISSUED + 1).unwrap(); }
            "release_owner" => fixture.journal.release_owner(&fixture.owner, ISSUED + 1).unwrap(),
            "transfer_owner" => { fixture.journal.transfer_owner(&fixture.owner, "other-owner", ISSUED + 1).unwrap(); }
            _ => { fixture.journal.advance_run_epoch(&fixture.owner, "run", fixture.grant.run_epoch, ISSUED + 1).unwrap(); }
        }
        assert!(read.authorize_host_connect(&fixture.token, &fixture.binding, EXPIRY, ISSUED + 2).is_err(), "{action}");
    }
}
