//! Cargo builds the exact standalone helper against this invocation's engine.
//! Normal CI probes its build identity only. Mount qualification requires an
//! explicitly owned disposable VM; never mount over a developer checkout.
#![cfg(target_os = "linux")]

#[test]
fn mounted_driver_compiles_without_touching_host_mounts() {
    use std::{fs, path::PathBuf, process::Command};
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_rudder-vfs-qualifier"));
    let probe = Command::new(&binary).arg("--build-info").output().unwrap();
    assert!(
        probe.status.success(),
        "{}",
        String::from_utf8_lossy(&probe.stderr)
    );
    let info: serde_json::Value = serde_json::from_slice(&probe.stdout).unwrap();
    assert_eq!(info["qualification"], "paged_ranges_v2");
    let output = if let Some(path) = std::env::var_os("RUDDER_VFS_DRIVER_OUTPUT") {
        let output = PathBuf::from(path);
        assert!(
            output.is_absolute(),
            "qualification output must be explicitly absolute"
        );
        fs::copy(&binary, &output).unwrap();
        output
    } else {
        binary
    };
    println!("mounted_driver={}", output.display());
}
