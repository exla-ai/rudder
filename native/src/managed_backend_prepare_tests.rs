use super::*;
use rudder_engine::journal::OwnerFence;

fn fence() -> ManagedLaunchFence {
    ManagedLaunchFence {
        owner: OwnerFence { project_id: "project".into(), owner_id: "owner".into(), epoch: 3 },
        run_id: "reserved-run".into(), run_epoch: 7,
        attempt_id: "e3eb83c7-9aa4-4585-adeb-2be31c7f6adc".into(),
        request_id: "1b683899-9fab-4f20-a0d0-148ef23530de".into(),
    }
}

#[test]
fn codex_layout_defers_frontend_and_requires_selected_key_to_stay_host_only() {
    let fence = fence();
    let backend = ManagedBackendProfile::CodexAppServer {
        program: "/opt/runtime/codex".into(),
        arguments: vec!["--model".into(), "chosen-model".into(), "--".into(), "literal prompt".into()],
    };
    let mut selected = SelectedCredentials::api_key(
        "selected-codex".into(), crate::Backend::Codex, "fixture-host-only-key".into()).unwrap();
    selected.check_backend(&backend).unwrap();
    let layout = prepare_layout(&fence, &backend, "/opt/runtime/native",
        Path::new("/tmp/cx.sock"), Path::new("/owned/run/home")).unwrap();

    // The newly shared environment boundary refuses accidental pre-login copy.
    assert!(selected.guest_environment(&fence, &layout.reports).is_err());
    let key = selected.take_host_codex_key().unwrap().unwrap();
    assert_eq!(key.expose_for_login(), "fixture-host-only-key");
    assert_eq!(layout.execution_profile, "codex-app-server");
    assert_eq!(layout.frontend_start, ManagedFrontendStart::AfterHostApiKeyV1);
    assert_eq!(layout.command, ["/opt/runtime/codex", "--remote", "unix:///tmp/cx.sock",
        "--model", "chosen-model", "-c", "cli_auth_credentials_store=\"ephemeral\"",
        "--", "literal prompt"].map(str::to_owned).to_vec());
    let server = layout.server_command.as_ref().unwrap();
    assert_eq!(&server[server.len()-3..], ["app-server", "--listen", "unix:///tmp/cx.sock"]);
    assert!(!server.iter().any(|arg| arg == "literal prompt"));
    let environment = selected.guest_environment(&fence, &layout.reports).unwrap();
    assert_eq!(environment, BTreeMap::from([
        ("RUDDER_FABRIC_ATTEMPT_ID".into(), fence.attempt_id.clone()),
        ("RUDDER_DONE_FILE".into(), "/home/agent/.rudder-reports/done.json".into()),
        ("CODEX_HOME".into(), "/home/agent/.codex".into()),
    ]));
    for file in &layout.home_files {
        assert!(!file.contents.windows(key.expose_for_login().len())
            .any(|bytes| bytes == key.expose_for_login().as_bytes()));
    }
    assert!(selected.take_host_codex_key().is_err(), "one selected key transfers only once");
}

#[test]
fn claude_layout_keeps_selected_key_and_existing_guest_environment_with_immediate_frontend() {
    let fence = fence();
    let backend = ManagedBackendProfile::ClaudeSdk {
        node: "/usr/bin/node".into(), rudder_cli: "/opt/rudder/main.js".into(),
        claude: "/opt/runtime/claude".into(), arguments: vec!["--model".into(), "chosen-model".into()],
        initial_prompt: Some("exact initial prompt".into()),
    };
    let mut selected = SelectedCredentials::api_key(
        "selected-claude".into(), crate::Backend::Claude, "fixture-claude-key".into()).unwrap();
    selected.check_backend(&backend).unwrap();
    assert!(selected.take_host_codex_key().unwrap().is_none());
    let home = Path::new("/owned/execution/runs/execution/home");
    let layout = prepare_layout(&fence, &backend, "/opt/runtime/native",
        Path::new("/tmp/cx.sock"), home).unwrap();
    assert_eq!(layout.execution_profile, "claude-sdk");
    assert_eq!(layout.frontend_start, ManagedFrontendStart::Immediate);
    assert!(layout.server_command.is_none());
    assert_eq!(layout.command, ["/usr/bin/node", "/opt/rudder/main.js", "__claude-sdk",
        "--launch-file", "/home/agent/.rudder/claude-launch.json"].map(str::to_owned).to_vec());
    assert_eq!(layout.reports.host_done_file, home.join(".rudder-reports/done.json"));
    assert_eq!(layout.reports.host_signal_file, home.join(".rudder-reports/signal.json"));
    let environment = selected.guest_environment(&fence, &layout.reports).unwrap();
    assert_eq!(environment, BTreeMap::from([
        ("ANTHROPIC_API_KEY".into(), "fixture-claude-key".into()),
        ("RUDDER_FABRIC_ATTEMPT_ID".into(), fence.attempt_id.clone()),
        ("RUDDER_DONE_FILE".into(), "/home/agent/.rudder-reports/done.json".into()),
        ("CODEX_HOME".into(), "/home/agent/.codex".into()),
    ]), "preserve the existing Claude environment, including explicit CODEX_HOME");
    let launch = &layout.home_files.iter().find(|file|
        file.relative_path == ".rudder/claude-launch.json").unwrap().contents;
    let launch: serde_json::Value = serde_json::from_slice(launch).unwrap();
    assert_eq!(launch["initial_prompt"], "exact initial prompt");
    assert_eq!(launch["attempt_id"], fence.attempt_id);
    assert!(!launch.to_string().contains("fixture-claude-key"));
}

#[test]
fn mismatch_and_unsupported_oauth_cannot_select_an_alternate_credential() {
    let fence = fence();
    let backend = ManagedBackendProfile::CodexAppServer {
        program: "/opt/runtime/codex".into(), arguments: Vec::new(),
    };
    let selected = SelectedCredentials::api_key(
        "explicit-claude".into(), crate::Backend::Claude, "fixture-original-key".into()).unwrap();
    let reports = ManagedReportMapping::for_execution(Path::new("/owned/home"));
    let before = selected.guest_environment(&fence, &reports).unwrap();
    assert!(selected.check_backend(&backend).is_err());
    let error = match SelectedCredentials::oauth_profile("unsupported-oauth") {
        Ok(_) => panic!("OAuth selection must remain unsupported"), Err(error) => error,
    };
    assert_eq!(error.downcast_ref::<std::io::Error>().unwrap().kind(),
        std::io::ErrorKind::Unsupported);
    assert_eq!(selected.guest_environment(&fence, &reports).unwrap(), before);
}
