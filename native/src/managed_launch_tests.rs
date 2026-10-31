use super::*;
use rudder_engine::protocol::ObjectKind;

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("rudder-managed-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        Self(path)
    }
}
impl Drop for Fixture { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); } }
struct EndpointFixture(PathBuf);
impl EndpointFixture {
    fn new() -> Self {
        let path=PathBuf::from("/tmp").join(format!("rme-{}",&uuid::Uuid::new_v4().simple().to_string()[..12]));
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap(); Self(path.canonicalize().unwrap())
    }
}
impl Drop for EndpointFixture { fn drop(&mut self) { let _=fs::remove_dir_all(&self.0); } }
fn fence() -> ManagedLaunchFence {
    ManagedLaunchFence { owner: OwnerFence { project_id: "project".into(), owner_id: "owner".into(), epoch: 1 },
        run_id: "run".into(), run_epoch: 1, attempt_id: uuid::Uuid::new_v4().to_string(), request_id: uuid::Uuid::new_v4().to_string() }
}
fn assignment(fence: &ManagedLaunchFence) -> WorkspaceAssignment {
    let reference = |kind, bytes: &[u8]| ObjectRef::from_bytes(kind, bytes);
    WorkspaceAssignment { format_version: 1, fence: fence.clone(),
        runnable_root: reference(ObjectKind::RunnableRoot, b"runnable"),
        source: reference(ObjectKind::SourceRoot, b"source"), seed: reference(ObjectKind::Directory, b"seed"),
        observation: ObservationBinding { format_version: 1,
            attempt: AttemptContext { execution_id: fence.attempt_id.clone(), operation_id: uuid::Uuid::new_v4().to_string() },
            view_id: uuid::Uuid::new_v4().to_string(), owner: fence.owner_epoch(), namespace: uuid::Uuid::new_v4().to_string(),
            profile: reference(ObjectKind::Blob, b"profile"), exclusion_policy_digest: "a".repeat(64) },
        host_context: ProtectedHostContextRef { format_version: 1, path: "/private/launch.json".into(),
            sha256: "b".repeat(64), size_bytes: 1 } }
}
fn private_fixture() -> (crate::managed_store::tests::Fixture, crate::managed_store::ManagedStore, ManagedLaunchFence) {
    let mut fixture=crate::managed_store::tests::Fixture::new(); let store=fixture.prepare(); let mut fence=fence(); fence.owner=fixture.owner.clone();
    fence.run_epoch=fixture.journal.observe_run_attempt(&fence.owner,&fence.run_id,&fence.attempt_id,2).unwrap();
    store.run(&fence.run_id).unwrap().persist_reservation(&ManagedRunSource::Reserved { fence:fence.clone(),
        runnable_root:ObjectRef::from_bytes(ObjectKind::RunnableRoot,b"runnable") }).unwrap();
    (fixture,store,fence)
}

#[test]
fn assignment_reconciles_exact_request_without_widening_or_reminting_identity() {
    let (fixture,_store,fence) = private_fixture(); let assignment = assignment(&fence);
    persist_assignment(&fixture.home, &assignment).unwrap();
    persist_assignment(&fixture.home, &assignment).unwrap();
    assert_eq!(read_assignment(&fixture.home, &fence).unwrap(), Some(assignment.clone()));
    assert_eq!(assignment_for_attempt(&fixture.home, &fence.owner, &fence.run_id, fence.run_epoch, &fence.attempt_id).unwrap(), Some(assignment.clone()));
    let mut changed = assignment.clone(); changed.observation.namespace = uuid::Uuid::new_v4().to_string();
    assert!(persist_assignment(&fixture.home, &changed).is_err());
    let mut other = fence.clone(); other.request_id = uuid::Uuid::new_v4().to_string();
    assert!(read_assignment(&fixture.home, &other).is_err());
    other = fence.clone(); other.run_epoch += 1;
    assert!(read_assignment(&fixture.home, &other).is_err());
    other = fence.clone(); other.attempt_id = uuid::Uuid::new_v4().to_string();
    assert!(read_assignment(&fixture.home, &other).unwrap().is_none());
}

#[test]
fn managed_inventory_assignment_reads_established_location_without_opening_journal() {
    let (mut fixture,store,fence)=private_fixture(); let assigned=assignment(&fence);
    persist_assignment(&fixture.home,&assigned).unwrap(); let location=store.run(&fence.run_id).unwrap();
    // Close the owned test connection, then make a new journal open impossible.
    // This tests the immutable read only, never treating the saved fence as live.
    let replacement=Journal::open(fixture.home.join("replacement.sqlite")).unwrap();
    drop(std::mem::replace(&mut fixture.journal,replacement));
    let path=fixture.home.join("fabric/journal.sqlite"); fs::rename(&path,fixture.home.join("offline.sqlite")).unwrap(); fs::create_dir(&path).unwrap();
    assert_eq!(assignment_for_location(&location,&fence.owner,&fence.run_id,fence.run_epoch,&fence.attempt_id).unwrap(),Some(assigned.clone()));
    let mut other=fence.owner.clone(); other.epoch+=1;
    assert!(assignment_for_location(&location,&other,&fence.run_id,fence.run_epoch,&fence.attempt_id).is_err());
    assert!(assignment_for_location(&location,&fence.owner,"foreign-run",fence.run_epoch,&fence.attempt_id).is_err());
    assert!(assignment_for_location(&location,&fence.owner,&fence.run_id,fence.run_epoch+1,&fence.attempt_id).is_err());
    let assignment_path=assignment_path(&fixture.home,&fence).unwrap(); fs::remove_file(&assignment_path).unwrap();
    assert!(assignment_for_location(&location,&fence.owner,&fence.run_id,fence.run_epoch,&fence.attempt_id).unwrap().is_none());
    fs::write(&assignment_path,b"malformed").unwrap(); fs::set_permissions(&assignment_path,fs::Permissions::from_mode(0o600)).unwrap();
    assert!(assignment_for_location(&location,&fence.owner,&fence.run_id,fence.run_epoch,&fence.attempt_id).is_err()); assert!(path.is_dir());
}

#[test]
fn assignment_cannot_rebind_owner_execution_or_object_kinds() {
    let fence = fence(); let value = assignment(&fence);
    let mut changed = value.clone(); changed.observation.owner.epoch += 1;
    assert!(changed.validate(&fence).is_err());
    changed = value.clone(); changed.observation.attempt.execution_id = uuid::Uuid::new_v4().to_string();
    assert!(changed.validate(&fence).is_err());
    changed = value; changed.runnable_root.kind = ObjectKind::Checkpoint;
    assert!(changed.validate(&fence).is_err());
}

#[test]
fn durable_managed_source_markers_remain_recovery_evidence_and_invalid_presence_is_quarantined() {
    let fixture = Fixture::new(); let fence = fence();
    let reserved = ManagedRunSource::Reserved { fence: fence.clone(), runnable_root: ObjectRef::from_bytes(ObjectKind::RunnableRoot, b"owned") };
    let assigned = ManagedRunSource::Assigned { assignment: Box::new(assignment(&fence)) };
    for source in [reserved, assigned] {
        let marker = serde_json::to_value(&source).unwrap();
        let record = serde_json::json!({"id":"run","task":"owned task","mode":"execute","status":"done", "managedLocal":marker,
            "session":{"nativeSessionId":"saved-session-is-not-liveness"}});
        let run = crate::gitio::agent_from_run_record(&fixture.0, record).unwrap();
        assert_eq!(run.managed_local, Some(ManagedRunSource::Quarantined));
        assert_eq!(run.status, super::super::AgentStatus::Orphaned);
        assert!(!crate::launch::can_resume_agent(&run));
        assert!(crate::gitio::save_native_run_record(&fixture.0, &run).is_err());
        assert!(!crate::gitio::native_run_dir(&fixture.0,"run").join("run.json").exists());
    }
    let mut invalid = serde_json::to_value(ManagedRunSource::Reserved { fence, runnable_root: ObjectRef::from_bytes(ObjectKind::RunnableRoot, b"owned") }).unwrap();
    invalid["unknown_authority"] = serde_json::json!(true);
    for marker in [serde_json::Value::Null, serde_json::json!({}), invalid] {
        let record = serde_json::json!({"id":"run","task":"owned task","status":"running", "managedLocal":marker});
        let run = crate::gitio::agent_from_run_record(&fixture.0, record).unwrap();
        assert_eq!(run.managed_local, Some(ManagedRunSource::Quarantined));
        assert_eq!(run.status, super::super::AgentStatus::Orphaned);
        assert!(!crate::launch::can_resume_agent(&run));
    }
    let legacy = crate::gitio::agent_from_run_record(&fixture.0, serde_json::json!({"id":"legacy","task":"old task"})).unwrap();
    assert!(legacy.managed_local.is_none());
}

#[test]
fn fsynced_reservation_survives_lost_run_projection_and_refuses_conflicting_requests() {
    let fixture = Fixture::new(); let fence = fence();
    let reserved = ManagedRunSource::Reserved { fence:fence.clone(), runnable_root:ObjectRef::from_bytes(ObjectKind::RunnableRoot, b"owned") };
    let path = crate::gitio::native_run_dir(&fixture.0, "run").join("managed-local.json");
    persist_reservation(&path, &reserved).unwrap(); persist_reservation(&path, &reserved).unwrap();
    let run = crate::gitio::agent_from_run_record(&fixture.0, serde_json::json!({"id":"run","task":"owned","status":"done",
        "session":{"nativeSessionId":"old-host-session"}})).unwrap();
    assert_eq!(run.managed_local, Some(ManagedRunSource::Quarantined));
    assert_eq!(run.status, super::super::AgentStatus::Orphaned);
    assert!(!crate::launch::can_resume_agent(&run));
    let mut changed = fence; changed.request_id = uuid::Uuid::new_v4().to_string();
    let changed = ManagedRunSource::Reserved { fence:changed, runnable_root:ObjectRef::from_bytes(ObjectKind::RunnableRoot, b"owned") };
    assert!(persist_reservation(&path, &changed).is_err());
    let conflict = crate::gitio::agent_from_run_record(&fixture.0, serde_json::json!({"id":"run","task":"owned","managedLocal":changed})).unwrap();
    assert_eq!(conflict.managed_local, Some(ManagedRunSource::Quarantined));
    fs::write(path, b"not a valid recovery marker").unwrap();
    let malformed = crate::gitio::agent_from_run_record(&fixture.0, serde_json::json!({"id":"run","task":"owned"})).unwrap();
    assert_eq!(malformed.managed_local, Some(ManagedRunSource::Quarantined));
}

#[test]
fn experimental_missing_and_corrupt_run_projections_remain_visible_without_import_or_rewrite() {
    let fixture=Fixture::new(); let reserved=ManagedRunSource::Reserved { fence:fence(), runnable_root:ObjectRef::from_bytes(ObjectKind::RunnableRoot,b"owned") };
    let directory=crate::gitio::native_run_dir(&fixture.0,"run"); let marker=directory.join("managed-local.json");
    persist_reservation(&marker,&reserved).unwrap(); let original=fs::read(&marker).unwrap();
    for corrupt in [false,true] {
        if corrupt { fs::write(directory.join("run.json"),b"partial JSON").unwrap(); }
        let rows=crate::gitio::load_persisted_agents(&fixture.0); assert_eq!(rows.len(),1);
        assert_eq!(rows[0].managed_local,Some(ManagedRunSource::Quarantined)); assert!(rows[0].managed_store.is_none());
        assert_eq!(rows[0].status,super::super::AgentStatus::Orphaned);
        assert!(crate::gitio::save_native_run_record(&fixture.0,&rows[0]).is_err());
        assert_eq!(fs::read(&marker).unwrap(),original);
    }
}

#[test]
fn managed_transient_endpoints_require_explicit_short_private_parent_and_exact_binding() {
    let fixture=Fixture::new(); let endpoints_parent=EndpointFixture::new(); let parent=endpoints_parent.0.clone(); let fence=fence();
    let public=fixture.0.join("public"); fs::DirBuilder::new().mode(0o755).create(&public).unwrap();
    assert!(validate_endpoint_parent(&public,&fence).is_err()); assert!(!public.join(&fence.request_id).exists());
    let alias=fixture.0.join("alias"); std::os::unix::fs::symlink(&parent,&alias).unwrap(); assert!(validate_endpoint_parent(&alias,&fence).is_err());
    let long=parent.join("long".repeat(30)); fs::DirBuilder::new().mode(0o700).create(&long).unwrap(); assert!(validate_endpoint_parent(&long,&fence).is_err());
    let permanent=fixture.0.join("permanent"); fs::create_dir(&permanent).unwrap(); fs::write(permanent.join("history"),b"retain").unwrap();
    let target=permanent.join("e/runs/attempt/tmp/cx.sock");
    let endpoints=prepare_endpoints(&parent,&fence,&permanent,target.clone()).unwrap();
    assert!(endpoints.root.join("workspace/workspace.sock").as_os_str().as_encoded_bytes().len()<100);
    assert_eq!(fs::read_link(endpoints.root.join("backend.sock")).unwrap(),target);
    let endpoint_root=endpoints.root.clone(); retire_endpoints(endpoints).unwrap();
    assert!(!endpoint_root.exists()); assert_eq!(fs::read(permanent.join("history")).unwrap(),b"retain");
}

#[test]
fn managed_endpoint_cleanup_refuses_changed_marker_and_never_deletes_permanent_state() {
    let fixture=Fixture::new(); let endpoints_parent=EndpointFixture::new(); let parent=endpoints_parent.0.clone();
    let permanent=fixture.0.join("permanent"); fs::create_dir(&permanent).unwrap(); let marker=b"owned preparation".to_vec(); fs::write(permanent.join("preparation.json"),&marker).unwrap();
    let endpoints=prepare_endpoints(&parent,&fence(),&permanent,permanent.join("cx.sock")).unwrap(); let root=endpoints.root.clone(); let original=endpoints.marker.clone();
    fs::write(root.join("owner.json"),b"changed").unwrap();
    assert!(retire_endpoints(ManagedEndpoints { root:root.clone(),marker:original.clone(),backend_target:permanent.join("cx.sock") }).is_err());
    assert!(root.exists()); fs::write(root.join("owner.json"),original).unwrap();
    let retirement=Retirement { root:permanent.clone(),marker,pins:vec!["permanent-pin".into()],adopted:true,endpoints:Some(endpoints),endpoints_safe:true };
    retire_unused(retirement).unwrap(); assert!(!root.exists()); assert!(permanent.join("preparation.json").exists());
}

#[test]
fn assignment_files_refuse_symlinks_and_shared_modes() {
    let (fixture,_store,fence) = private_fixture(); let assignment = assignment(&fence);
    persist_assignment(&fixture.home, &assignment).unwrap();
    let path = assignment_path(&fixture.home, &fence).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(read_assignment(&fixture.home, &fence).is_err());
    fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink("missing", path).unwrap();
    assert!(read_assignment(&fixture.home, &fence).is_err());
}

#[test]
fn current_launch_fence_requires_the_already_reserved_instance() {
    let fixture = Fixture::new(); let mut journal = Journal::open(fixture.0.join("journal.sqlite")).unwrap();
    let owner = journal.create_project("project", "owner", "base", 0).unwrap();
    let mut fence = fence(); fence.owner = owner;
    assert!(fence.check_current(&journal).is_err());
    fence.run_epoch = journal.observe_run_attempt(&fence.owner, &fence.run_id, &fence.attempt_id, 1).unwrap();
    fence.check_current(&journal).unwrap();
    journal.observe_run_attempt(&fence.owner, &fence.run_id, &uuid::Uuid::new_v4().to_string(), 2).unwrap();
    assert!(fence.check_current(&journal).is_err());
}

#[test]
fn selected_credentials_do_not_copy_environment_or_guess_an_oauth_profile() {
    let selected = SelectedCredentials::api_key("selected".into(), super::super::Backend::Codex, "fixture-secret".into()).unwrap();
    assert_eq!(selected.profile_id, "selected");
    assert_eq!(selected.environment.keys().map(String::as_str).collect::<Vec<_>>(), ["OPENAI_API_KEY"]);
    let error = match SelectedCredentials::oauth_profile("selected-oauth") { Ok(_) => panic!("unqualified refresh must refuse"), Err(error) => error };
    assert_eq!(error.downcast_ref::<std::io::Error>().unwrap().kind(), std::io::ErrorKind::Unsupported);
}

#[test]
fn selected_codex_key_stays_host_only_and_forces_ephemeral_store_before_literal_prompt() {
    let mut selected = SelectedCredentials::api_key("selected".into(), super::super::Backend::Codex, "fixture-host-only-key".into()).unwrap();
    let key = selected.take_host_codex_key().unwrap().unwrap();
    assert_eq!(key.expose_for_login(), "fixture-host-only-key"); assert!(selected.environment.is_empty());
    let backend = ManagedBackendProfile::CodexAppServer { program:"/opt/runtime/codex".into(), arguments:vec![
        "-c".into(), "cli_auth_credentials_store=\"file\"".into(), "--".into(), "literal prompt".into(), "--config=cli_auth_credentials_store=\"file\"".into()] };
    let (terminal, server, files) = prepare_backend(&fence(), &backend, "/opt/runtime/native", Path::new("/tmp/cx.sock")).unwrap();
    let delimiter = terminal.iter().position(|arg|arg == "--").unwrap();
    assert_eq!(&terminal[delimiter-2..delimiter], ["-c", "cli_auth_credentials_store=\"ephemeral\""]);
    let server = server.unwrap();
    assert_eq!(server.iter().filter(|arg|arg.contains("cli_auth_credentials_store")).last().unwrap(), "cli_auth_credentials_store=\"ephemeral\"");
    assert!(!server.iter().any(|arg|arg == "literal prompt"));
    assert_eq!(files.iter().find(|file|file.relative_path==".codex/config.toml").unwrap().contents,
        b"cli_auth_credentials_store = \"ephemeral\"\n", "explicit CODEX_HOME must exist before app-server startup");
    for bytes in files.iter().map(|file|file.contents.as_slice()) {
        assert!(!bytes.windows(key.expose_for_login().len()).any(|value|value == key.expose_for_login().as_bytes()));
    }
}

#[test]
fn backend_programs_require_explicit_read_only_image_projection() {
    let runtime = [ReadOnlyRuntime { source:"/opt/runtime".into(), guest_path:"/opt/runtime".into() }];
    validate_runtime_program("/opt/runtime/codex", &runtime).unwrap();
    for path in ["codex", "/workspace/backend", "/home/agent/claude", "/opt/runtime/../private/claude"] {
        assert!(validate_runtime_program(path, &runtime).is_err());
    }
    assert!(validate_runtime_program("/opt/runtime/codex", &[]).is_err());
}

#[test]
fn protected_credentials_are_explicit_bounded_host_roots_including_identity_home() {
    let roots = selected_protected_roots(Path::new("/owned/identity"), &["/owned/selected-vault".into(), "/owned/sibling-vault".into()]).unwrap();
    assert_eq!(roots, [PathBuf::from("/owned/identity"), "/owned/selected-vault".into(), "/owned/sibling-vault".into()]);
    assert!(selected_protected_roots(Path::new("/owned/identity"), &vec![PathBuf::from("/owned/vault");33]).is_err());
    for root in ["relative", "/", "/owned/../vault"] { assert!(selected_protected_roots(Path::new("/owned/identity"), &[root.into()]).is_err()); }
}

#[test]
#[cfg(target_os = "linux")]
fn declared_runtime_cannot_include_selected_or_sibling_credentials_under_public_image_roots() {
    let fixture = Fixture::new();
    let mut spec = ExecutionSpec { profile: None, format_version:1, execution_id:uuid::Uuid::new_v4().to_string(), operation_id:uuid::Uuid::new_v4().to_string(),
        execution_root:fixture.0.join("executions"), manager_root:fixture.0.join("manager"), store_root:fixture.0.join("store"),
        authority_roots:selected_protected_roots(&fixture.0.join("identity"), &[]).unwrap(), view_id:uuid::Uuid::new_v4().to_string(),
        owner:OwnerEpoch { owner:"owner".into(), epoch:1 }, command:vec!["/usr/bin/true".into()], environment:Default::default(),
        runtime:vec![ReadOnlyRuntime { source:"/usr".into(), guest_path:"/usr".into() }], home_files:Vec::new(),
        broker_directory:None, broker_binding:None, workspace_observation:None, timeout_seconds:1 };
    spec.validate().unwrap();
    spec.authority_roots = selected_protected_roots(Path::new("/usr/share/protected-identity-fixture-not-created"), &[]).unwrap();
    assert!(spec.validate().is_err(), "binding a runtime ancestor cannot expose identity home");
    spec.authority_roots = selected_protected_roots(&fixture.0.join("identity"), &["/usr/share/protected-credential-fixture-not-created".into()]).unwrap();
    assert!(spec.validate().is_err(), "binding a runtime ancestor cannot expose a selected/sibling credential vault");
}

#[test]
fn managed_completion_paths_point_only_to_this_execution_home_and_remain_candidate_data() {
    let home = Path::new("/owned/request/e/runs/reserved-execution/home");
    let mapping = ManagedReportMapping::for_execution(home);
    assert_eq!(mapping.host_done_file, home.join(".rudder-reports/done.json"));
    assert_eq!(mapping.host_signal_file, home.join(".rudder-reports/signal.json"));
    assert_eq!(mapping.guest_done_file, Path::new("/home/agent/.rudder-reports/done.json"));
    let claude = crate::signals::claude_settings_json(&mapping.guest_signal_file, false);
    let codex = crate::signals::codex_notify_script(&mapping.guest_signal_file);
    for wiring in [&claude, &codex] {
        assert!(wiring.contains("/home/agent/.rudder-reports/signal.json"));
        assert!(!wiring.contains("/owned/") && !wiring.contains("fabric/agent-brokers"));
    }
}

#[test]
fn backend_mapping_keeps_mcp_hooks_and_sdk_config_in_the_owned_guest_home() {
    let fence = fence();
    let codex = ManagedBackendProfile::CodexAppServer { program: "/opt/runtime/codex".into(),
        arguments: vec!["--config".into(), "model_reasoning_effort=\"low\"".into(),
            "--config=notify=[\"/legacy/host-hook\"]".into(), "--model".into(), "chosen-model".into(), "hello".into()] };
    let (terminal, server, files) = prepare_backend(&fence, &codex, "/opt/rudder/native", Path::new("/tmp/cx.sock")).unwrap();
    assert_eq!(&terminal[..3], ["/opt/runtime/codex", "--remote", "unix:///tmp/cx.sock"]);
    let server = server.unwrap();
    assert!(server.windows(2).any(|args| args == ["--config", "model_reasoning_effort=\"low\""]));
    assert_eq!(server.last().unwrap(), "unix:///tmp/cx.sock");
    let own_notify = server.iter().position(|arg| arg == "notify=[\"/bin/sh\",\"/home/agent/.rudder/notify.sh\"]").unwrap();
    let earlier_notify = server.iter().position(|arg| arg.starts_with("--config=notify=")).unwrap();
    assert!(own_notify > earlier_notify, "owned report hook wins on the actual server");
    assert!(!server.iter().any(|arg| arg == "hello"), "task input belongs only to the remote frontend");
    let mcp: serde_json::Value = serde_json::from_slice(&files.iter().find(|file|file.relative_path == ".rudder/mcp.json").unwrap().contents).unwrap();
    assert_eq!(mcp["mcpServers"]["rudder_agents"]["command"], "/opt/rudder/native");
    assert!(mcp.to_string().contains("/run/rudder/agent/identity.json"));
    assert!(!mcp.to_string().contains("/workspace/home"));

    let claude = ManagedBackendProfile::ClaudeSdk { node: "/usr/bin/node".into(), rudder_cli: "/opt/rudder/main.js".into(),
        claude: "/opt/runtime/claude".into(), arguments: vec!["--model".into(), "chosen-model".into()], initial_prompt: Some("exact prompt".into()) };
    let (terminal, server, files) = prepare_backend(&fence, &claude, "/opt/rudder/native", Path::new("/tmp/cx.sock")).unwrap();
    assert!(server.is_none());
    assert_eq!(terminal.last().unwrap(), "/home/agent/.rudder/claude-launch.json");
    let launch: serde_json::Value = serde_json::from_slice(&files.iter().find(|file|file.relative_path == ".rudder/claude-launch.json").unwrap().contents).unwrap();
    assert_eq!(launch["attempt_id"], fence.attempt_id);
    assert_eq!(launch["socket_path"], "/tmp/cx.sock");
    assert_eq!(launch["claude_args"], serde_json::json!(["--model", "chosen-model", "--mcp-config", "/home/agent/.rudder/mcp.json", "--settings", "/home/agent/.rudder/claude-settings.json"]));
    assert_eq!(launch["initial_prompt"], "exact prompt");
    let nested = ManagedBackendProfile::CodexAppServer { program: "/opt/runtime/codex".into(), arguments: vec!["--remote=unix:///peer.sock".into()] };
    assert!(prepare_backend(&fence, &nested, "/opt/rudder/native", Path::new("/tmp/cx.sock")).is_err());
}
