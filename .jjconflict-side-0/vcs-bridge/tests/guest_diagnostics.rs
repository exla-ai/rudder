#![cfg(feature = "cli")]
use rudder_engine::protocol::OwnerEpoch;
use rudder_vcs_bridge::command_transport::{ADAPTER_VERSION, CommandBinding, CommandClientConfig};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let config = CommandClientConfig {
        format_version: 1,
        socket: root.path().join("no-server.sock"),
        identity_file: root.path().join("no-identity.json"),
        binding: CommandBinding {
            format_version: 1,
            project_id: "p".into(),
            owner: OwnerEpoch {
                owner: "engine".into(),
                epoch: 1,
            },
            run_id: "r".into(),
            run_epoch: 1,
            attempt_id: uuid::Uuid::new_v4().to_string(),
            view_id: uuid::Uuid::new_v4().to_string(),
        },
    };
    let path = root.path().join("client.json");
    fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    (root, path)
}
#[test]
fn scoped_help_and_version_are_offline_and_create_no_history_receipt_or_observation() {
    let (root, path) = fixture();
    for arg in ["--version", "--help"] {
        let output = Command::new(env!("CARGO_BIN_EXE_rudder-jj"))
            .arg(arg)
            .env("RUDDER_JJ_CLIENT_CONFIG", &path)
            .env_remove("RUDDER_JJ_HOST_CONTEXT")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(text.starts_with(&format!("rudder-jj {ADAPTER_VERSION}\n")));
        if arg == "--help" {
            for command in [
                "status",
                "diff",
                "log",
                "new -m",
                "describe -m",
                "edit FULL-COMMIT-ID",
            ] {
                assert!(text.contains(command));
            }
            for unsupported in [
                "restore",
                "rebase",
                "--config",
                "--repository",
                "--template",
            ] {
                assert!(!text.contains(unsupported));
            }
        }
        assert_eq!(
            fs::read_dir(root.path())
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect::<Vec<_>>(),
            vec![std::ffi::OsString::from("client.json")]
        );
    }
    // Runtime commands still require actual current authority/transport.
    let status = Command::new(env!("CARGO_BIN_EXE_rudder-jj"))
        .arg("status")
        .env("RUDDER_JJ_CLIENT_CONFIG", &path)
        .env_remove("RUDDER_JJ_HOST_CONTEXT")
        .output()
        .unwrap();
    assert!(!status.status.success());
}
#[test]
fn diagnostic_does_not_accept_extra_flags_unsafe_config_or_a_host_role() {
    let (_root, path) = fixture();
    for args in [
        vec!["--help", "--config", "ui.editor=anything"],
        vec!["--version", "status"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_rudder-jj"))
            .args(args)
            .env("RUDDER_JJ_CLIENT_CONFIG", &path)
            .env_remove("RUDDER_JJ_HOST_CONTEXT")
            .output()
            .unwrap();
        assert!(!output.status.success());
    }
    let output = Command::new(env!("CARGO_BIN_EXE_rudder-jj"))
        .arg("--help")
        .env("RUDDER_JJ_CLIENT_CONFIG", &path)
        .env("RUDDER_JJ_HOST_CONTEXT", "/must-not-be-read")
        .output()
        .unwrap();
    assert!(!output.status.success());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_rudder-jj"))
        .arg("--help")
        .env("RUDDER_JJ_CLIENT_CONFIG", &path)
        .env_remove("RUDDER_JJ_HOST_CONTEXT")
        .output()
        .unwrap();
    assert!(!output.status.success());
}
