use rudder_engine::{
    agent_api::{AgentApi, AgentIdentityFile},
    journal::{CapabilityGrant, Journal, OwnerFence},
    protocol::{canonical_json, ObjectKind, ObjectRef, OwnerEpoch},
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
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
};
fn id() -> String {
    uuid::Uuid::new_v4().to_string()
}
struct Fixture {
    temporary: tempfile::TempDir,
    journal: Journal,
    owner: OwnerFence,
    binding: PartialBinding,
}
impl Fixture {
    fn new() -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let mut journal = Journal::open(temporary.path().join("journal.sqlite")).unwrap();
        let owner = journal.create_project("p", "engine", "base", 0).unwrap();
        for run in ["a", "b"] {
            journal.register_run(&owner, run, 0).unwrap();
        }
        let execution = id();
        journal
            .observe_run_attempt(&owner, "a", &execution, 0)
            .unwrap();
        let attempt = AttemptBinding {
            project_id: "p".into(),
            run_id: "a".into(),
            run_epoch: 1,
            owner: OwnerEpoch {
                owner: owner.owner_id.clone(),
                epoch: owner.epoch,
            },
            execution_id: execution,
            operation_id: id(),
            view_id: id(),
        };
        let binding = PartialBinding {
            protocol: PROTOCOL.into(),
            format_version: 1,
            launch_request_digest: "a".repeat(64),
            root: RootBinding {
                scope: CacheScope {
                    provider_id: "fixture".into(),
                    tenant_id: id(),
                },
                root_id: "root".into(),
                root: ObjectRef::from_bytes(ObjectKind::RunnableRoot, b"fixture"),
            },
            observation: ObservationBinding {
                format_version: 1,
                attempt: AttemptContext {
                    execution_id: attempt.execution_id.clone(),
                    operation_id: attempt.operation_id.clone(),
                },
                view_id: attempt.view_id.clone(),
                owner: attempt.owner.clone(),
                namespace: id(),
                profile: ObjectRef::from_bytes(ObjectKind::Blob, b"fixture"),
                exclusion_policy_digest: exclusion_policy_digest(&ImportPolicy::default()).unwrap(),
            },
            attempt,
        };
        Self {
            temporary,
            journal,
            owner,
            binding,
        }
    }
    fn grant(run: &str, operation: &str, scope: String) -> CapabilityGrant {
        CapabilityGrant {
            principal: format!("fixture:{run}"),
            run_id: run.into(),
            run_epoch: 1,
            operations: BTreeSet::from([operation.into()]),
            scopes: BTreeSet::from([scope]),
            recipients: BTreeMap::new(),
            topics: BTreeSet::new(),
            expires_at_ms: None,
        }
    }
    fn token(&mut self, operation: &str, scope: String) -> String {
        self.journal
            .issue_capability(&self.owner, &Self::grant("a", operation, scope), 0)
            .unwrap()
    }
    fn identity(&self, token: String) -> AgentIdentityFile {
        AgentIdentityFile {
            format_version: 1,
            project_id: "p".into(),
            run_id: "a".into(),
            run_epoch: 1,
            owner_epoch: self.owner.epoch,
            token,
        }
    }
    fn file(&self, identity: &AgentIdentityFile) -> PathBuf {
        let path = self.temporary.path().join("identity.json");
        fs::write(&path, canonical_json(identity).unwrap()).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        path
    }
}
#[test]
fn partial_right_and_exact_family_scope_are_independently_required_without_widening_complete() {
    let mut f = Fixture::new();
    let scope = partial_scope(&f.binding).unwrap();
    let complete = rudder_engine::execution::workspace_scope(&f.binding.observation).unwrap();
    let wrong_right = f.token("workspace:observe", scope.clone());
    assert!(f
        .journal
        .authorize_partial_workspace(&wrong_right, &f.binding, 1)
        .is_err());
    let wrong_scope = f.token(REQUIRED_OPERATION, complete.clone());
    assert!(f
        .journal
        .authorize_partial_workspace(&wrong_scope, &f.binding, 1)
        .is_err());
    let allowed = f.token(REQUIRED_OPERATION, scope);
    let current = f
        .journal
        .authorize_partial_workspace(&allowed, &f.binding, 1)
        .unwrap();
    assert_eq!(current.project_id, "p");
    assert_eq!(current.run_id, "a");
    assert!(f
        .journal
        .authorize_workspace(&allowed, &f.binding.observation, 1)
        .is_err());
    let legacy = f.token("workspace:observe", complete);
    f.journal
        .authorize_workspace(&legacy, &f.binding.observation, 1)
        .unwrap();
    assert!(f
        .journal
        .authorize_partial_workspace(&legacy, &f.binding, 1)
        .is_err());
}
#[test]
fn partial_scope_binds_full_launch_and_owner_namespace_profile_root_and_attempt() {
    let mut f = Fixture::new();
    let token = f.token(REQUIRED_OPERATION, partial_scope(&f.binding).unwrap());
    for change in 0..6 {
        let mut offered = f.binding.clone();
        match change {
            0 => offered.launch_request_digest = "b".repeat(64),
            1 => offered.observation.namespace = id(),
            2 => offered.observation.profile = ObjectRef::from_bytes(ObjectKind::Blob, b"changed"),
            3 => offered.root.root_id = "other-root".into(),
            4 => offered.attempt.run_id = "b".into(),
            5 => {
                offered.attempt.owner.epoch += 1;
                offered.observation.owner.epoch += 1;
            }
            _ => unreachable!(),
        }
        assert!(f
            .journal
            .authorize_partial_workspace(&token, &offered, 1)
            .is_err());
    }
}
#[test]
fn capability_scope_cannot_cross_project_run_owner_or_current_execution_even_when_explicitly_listed(
) {
    let mut f = Fixture::new();
    let scope = partial_scope(&f.binding).unwrap();
    f.journal
        .observe_run_attempt(&f.owner, "b", &f.binding.attempt.execution_id, 0)
        .unwrap();
    let other = f
        .journal
        .issue_capability(
            &f.owner,
            &Fixture::grant("b", REQUIRED_OPERATION, scope.clone()),
            0,
        )
        .unwrap();
    assert!(f
        .journal
        .authorize_partial_workspace(&other, &f.binding, 1)
        .is_err());
    let owner = f
        .journal
        .create_project("foreign", "engine", "base", 0)
        .unwrap();
    f.journal.register_run(&owner, "a", 0).unwrap();
    f.journal
        .observe_run_attempt(&owner, "a", &f.binding.attempt.execution_id, 0)
        .unwrap();
    let foreign = f
        .journal
        .issue_capability(
            &owner,
            &Fixture::grant("a", REQUIRED_OPERATION, scope.clone()),
            0,
        )
        .unwrap();
    assert!(f
        .journal
        .authorize_partial_workspace(&foreign, &f.binding, 1)
        .is_err());
    let current = f.token(REQUIRED_OPERATION, scope);
    f.journal.revoke_capability(&f.owner, &current, 1).unwrap();
    assert!(f
        .journal
        .authorize_partial_workspace(&current, &f.binding, 2)
        .is_err());
    let current = f.token(REQUIRED_OPERATION, partial_scope(&f.binding).unwrap());
    f.journal
        .observe_run_attempt(&f.owner, "a", &id(), 2)
        .unwrap();
    assert!(f
        .journal
        .authorize_partial_workspace(&current, &f.binding, 3)
        .is_err());
}
#[test]
fn partial_agent_api_rotation_is_current_and_not_a_model_tool() {
    let mut f = Fixture::new();
    let token = f.token(REQUIRED_OPERATION, partial_scope(&f.binding).unwrap());
    let identity = f.identity(token);
    let path = f.file(&identity);
    let api = AgentApi::open(f.temporary.path().join("journal.sqlite"), &path).unwrap();
    api.authorize_partial_workspace(&f.binding).unwrap();
    assert!(api
        .call(
            "authorize_partial_workspace",
            serde_json::json!({"binding":f.binding})
        )
        .is_err());
    f.journal
        .revoke_capability(&f.owner, &identity.token, 1)
        .unwrap();
    assert!(api.authorize_partial_workspace(&f.binding).is_err());
    let token = f.token(REQUIRED_OPERATION, partial_scope(&f.binding).unwrap());
    let next = f.identity(token);
    f.file(&next);
    api.authorize_partial_workspace(&f.binding).unwrap();
    f.journal
        .transfer_owner(&f.owner, "replacement", 3)
        .unwrap();
    assert!(api.authorize_partial_workspace(&f.binding).is_err());
}

#[test]
fn partial_agent_api_identity_fifo_cannot_block_before_regular_file_validation() {
    use std::os::unix::{ffi::OsStrExt, fs::OpenOptionsExt};
    let f = Fixture::new();
    let fifo = f.temporary.path().join("identity-fifo");
    let name = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    let journal = f.temporary.path().join("journal.sqlite");
    let path = fifo.clone();
    let (send, receive) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        let _ = send.send(AgentApi::open(journal, path).is_err());
    });
    let prompt = receive.recv_timeout(std::time::Duration::from_secs(1));
    if prompt.is_err() {
        let _writer = fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&fifo);
        let _ = receive.recv_timeout(std::time::Duration::from_secs(1));
    }
    if reader.is_finished() {
        reader.join().unwrap();
    }
    assert_eq!(prompt.ok(), Some(true));
}
