use super::*;
use std::sync::{atomic::AtomicBool, mpsc};

fn selected(backend: Backend) -> ManagedTaskOptIn {
    ManagedTaskOptIn::selected("fixture:selected-api".into(), backend, SelectedTaskCredentialKind::ApiKeyV1,
        ManagedTaskNetworkSelection::OrdinaryNetworkV1).unwrap()
}
fn target(backend: Backend) -> ManagedTaskTarget {
    ManagedTaskTarget::validate("new-execute-row".into(), "implement the explicit task".into(), "explicit-model".into(), backend,
        AgentMode::Execute, AgentStatus::Stopped).unwrap()
}
fn admitted(controller: &ManagedTaskController) -> ManagedTaskAdmission {
    controller.admit_target(target(Backend::Codex), selected(Backend::Codex)).unwrap()
}

#[test]
fn selected_oauth_is_an_explicit_unsupported_gate_before_any_selector_or_admission() {
    let controller = ManagedTaskController::new();
    for (backend, profile) in [(Backend::Codex, "openai-codex:default"), (Backend::Claude, "anthropic:claude-code")] {
        let error = ManagedTaskOptIn::selected(profile.into(), backend, SelectedTaskCredentialKind::OAuthV1,
            ManagedTaskNetworkSelection::OrdinaryNetworkV1).unwrap_err();
        assert_eq!(error.downcast_ref::<std::io::Error>().unwrap().kind(), std::io::ErrorKind::Unsupported);
        assert!(error.to_string().contains("selected OAuth profile"));
    }
    assert_eq!(controller.active.load(Ordering::Acquire), 0);
}

#[test]
fn explicit_profile_model_backend_and_task_input_are_bounded_without_defaults() {
    assert!(ManagedTaskOptIn::selected("".into(), Backend::Codex, SelectedTaskCredentialKind::ApiKeyV1,
        ManagedTaskNetworkSelection::OrdinaryNetworkV1).is_err());
    assert!(ManagedTaskOptIn::selected("fixture".into(), Backend::Opencode, SelectedTaskCredentialKind::ApiKeyV1,
        ManagedTaskNetworkSelection::OrdinaryNetworkV1).is_err());
    for run in ["", "..", "a/b", "a\\b", "control\n"] {
        assert!(ManagedTaskTarget::validate(run.into(), "task".into(), "model".into(), Backend::Codex,
            AgentMode::Execute, AgentStatus::Stopped).is_err());
    }
    for task in ["".to_string(), " ".into(), "contains\0nul".into(), "x".repeat(65537)] {
        assert!(ManagedTaskTarget::validate("run".into(), task, "model".into(), Backend::Codex,
            AgentMode::Execute, AgentStatus::Stopped).is_err());
    }
    assert!(ManagedTaskTarget::validate("run".into(), "task".into(), "".into(), Backend::Codex,
        AgentMode::Execute, AgentStatus::Stopped).is_err());
}

#[test]
fn only_a_stopped_execute_row_is_admitted_and_mode_status_are_bound() {
    for mode in [AgentMode::Plan, AgentMode::RudderPlan, AgentMode::ReviewAll,
        AgentMode::Main, AgentMode::OneOff, AgentMode::GamAdversarial] {
        assert!(ManagedTaskTarget::validate("run".into(), "task".into(), "model".into(), Backend::Codex,
            mode, AgentStatus::Stopped).is_err());
    }
    for status in [AgentStatus::Running, AgentStatus::Done, AgentStatus::Merged,
        AgentStatus::Failed, AgentStatus::Paused, AgentStatus::Orphaned, AgentStatus::Migrated] {
        assert!(ManagedTaskTarget::validate("run".into(), "task".into(), "model".into(), Backend::Codex,
            AgentMode::Execute, status).is_err());
    }
    let expected = target(Backend::Codex);
    let mut changed = expected.clone(); changed.mode = AgentMode::Plan;
    assert_ne!(expected, changed, "a queued row's mode is part of stale-result identity");
    let mut changed = expected.clone(); changed.status = AgentStatus::Paused;
    assert_ne!(expected, changed, "a queued row's status is part of stale-result identity");
}

#[test]
fn literal_task_and_exact_selected_model_are_bound_to_real_backend_layouts() {
    let mut row = target(Backend::Codex); row.task = "--config=cli_auth_credentials_store=file\nliteral task".into();
    let profile = ManagedTaskPrograms::Codex { program: "/opt/runtime/codex".into() }.bind(&row).unwrap();
    match profile {
        ManagedBackendProfile::CodexAppServer { program, arguments } => {
            assert_eq!(program, "/opt/runtime/codex");
            assert_eq!(arguments, vec!["--model".to_string(), row.model().to_string(), "--".to_string(), row.task().to_string()]);
        }
        _ => panic!("must remain a managed Codex profile"),
    }
    let row = target(Backend::Claude);
    let profile = ManagedTaskPrograms::Claude { node: "/usr/bin/node".into(), rudder_cli: "/opt/rudder/main.js".into(), claude: "/opt/runtime/claude".into() }.bind(&row).unwrap();
    match profile {
        ManagedBackendProfile::ClaudeSdk { arguments, initial_prompt, .. } => {
            assert_eq!(arguments, vec!["--model".to_string(), row.model().to_string()]); assert_eq!(initial_prompt.as_deref(), Some(row.task()));
        }
        _ => panic!("must remain a managed Claude SDK profile"),
    }
    assert!(ManagedTaskPrograms::Codex { program: "/opt/runtime/codex".into() }.bind(&row).is_err());
}

#[test]
fn selected_secret_owner_proves_the_selected_id_and_keeps_codex_key_host_owned() {
    let mut profile = SelectedTaskProfile::api_key(selected(Backend::Codex), "fixture-provider-key".into()).unwrap();
    assert_eq!(profile.selected.profile_id(), "fixture:selected-api");
    assert_eq!(profile.selected.backend(), Backend::Codex);
    let host_key = profile.credentials.take_host_codex_key().unwrap().unwrap();
    assert_eq!(host_key.expose_for_login(), "fixture-provider-key");
    // The existing owner enforces the one-shot take and zeroizes on Drop.
    assert!(profile.credentials.take_host_codex_key().is_err());
}

#[test]
fn preparation_slots_remain_charged_after_cancellation_until_actual_callback_return() {
    let controller = ManagedTaskController::new();
    let first = admitted(&controller); let second = admitted(&controller);
    assert!(controller.admit_target(target(Backend::Codex), selected(Backend::Codex)).is_err());
    let cancellation = first.cancellation();
    let (entered, saw_enter) = mpsc::sync_channel(1);
    let (release, released) = mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        let result = first.load_profile(|selection, _, _| {
            entered.send(()).unwrap(); released.recv().unwrap();
            SelectedTaskProfile::api_key(selection.clone(), "fixture-provider-key".into())
        });
        result.map(|_| ())
    });
    saw_enter.recv().unwrap(); cancellation.cancel();
    assert_eq!(controller.active.load(Ordering::Acquire), 2);
    assert!(controller.admit_target(target(Backend::Codex), selected(Backend::Codex)).is_err());
    release.send(()).unwrap(); assert!(worker.join().unwrap().is_err());
    assert_eq!(controller.active.load(Ordering::Acquire), 1);
    drop(second); assert_eq!(controller.active.load(Ordering::Acquire), 0);
}

#[test]
fn cancelled_queued_preparation_does_not_read_any_profile_or_renew_its_time() {
    let controller = ManagedTaskController::new(); let admission = admitted(&controller);
    let called = AtomicBool::new(false); admission.cancellation().cancel();
    assert!(admission.load_profile(|selection, _, _| {
        called.store(true, Ordering::Release);
        SelectedTaskProfile::api_key(selection.clone(), "fixture-provider-key".into())
    }).is_err());
    assert!(!called.load(Ordering::Acquire)); assert!(admission.budget.remaining().is_err());
    assert_eq!(controller.active.load(Ordering::Acquire), 1);
    drop(admission); assert_eq!(controller.active.load(Ordering::Acquire), 0);
}

#[test]
fn wrong_profile_or_backend_cannot_replace_the_explicit_selected_owner() {
    let controller = ManagedTaskController::new(); let admission = admitted(&controller);
    assert!(controller.admit_target(target(Backend::Codex), selected(Backend::Claude)).is_err());
    assert!(admission.load_profile(|_, _, _| SelectedTaskProfile::api_key(
        ManagedTaskOptIn::selected("another-api-profile".into(), Backend::Codex, SelectedTaskCredentialKind::ApiKeyV1,
            ManagedTaskNetworkSelection::OrdinaryNetworkV1).unwrap(), "fixture-key".into())).is_err());
}

#[test]
fn selector_failure_cannot_leak_credentials_in_its_error_chain() {
    let controller = ManagedTaskController::new(); let admission = admitted(&controller);
    let error = match admission.load_profile(|_, _, _| anyhow::bail!("DO_NOT_EXPOSE_FIXTURE_SECRET")) {
        Ok(_) => panic!("selector failure must refuse"), Err(error) => error,
    };
    assert!(!format!("{error:#}").contains("DO_NOT_EXPOSE_FIXTURE_SECRET"));
}

#[test]
fn new_entry_paths_never_infer_a_current_directory_or_credential_home() {
    for value in ["codex", "/runtime/../private", "/runtime/./native"] { assert!(!normalized(Path::new(value))); }
    assert!(normalized(Path::new("/opt/runtime/native")));
    let row = target(Backend::Codex);
    assert_eq!(row.run_id(), "new-execute-row"); assert_eq!(row.backend(), Backend::Codex);
}

#[test]
fn explicit_snapshot_intent_is_codex_only_and_selected_profile_metadata_decides_the_actual_owner() {
    let snapshot=ManagedTaskOptIn::selected("selected".into(),Backend::Codex,SelectedTaskCredentialKind::CodexOAuthAccessSnapshotV1,ManagedTaskNetworkSelection::OrdinaryNetworkV1).unwrap();
    assert!(snapshot.accepts_codex_snapshot());assert!(!snapshot.accepts_api_key());
    assert!(ManagedTaskOptIn::selected("selected".into(),Backend::Claude,SelectedTaskCredentialKind::CodexOAuthAccessSnapshotV1,ManagedTaskNetworkSelection::OrdinaryNetworkV1).is_err());
    let profile=ManagedTaskOptIn::selected("selected".into(),Backend::Codex,SelectedTaskCredentialKind::SelectedProfileV1,ManagedTaskNetworkSelection::OrdinaryNetworkV1).unwrap();
    assert!(profile.accepts_codex_snapshot());assert!(profile.accepts_api_key());
    let claude=ManagedTaskOptIn::selected("selected".into(),Backend::Claude,SelectedTaskCredentialKind::SelectedProfileV1,ManagedTaskNetworkSelection::OrdinaryNetworkV1).unwrap();
    assert!(!claude.accepts_codex_snapshot());
}
