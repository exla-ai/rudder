//! Copies/probes Cargo's exact standalone controller. This test never mounts a
//! filesystem, changes devices or provisions a worker.
#![cfg(target_os = "linux")]
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command,
};

fn qualifier() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_rudder-execution-qualifier"))
}
fn describe(binary: &Path) {
    let output = Command::new(binary).arg("--describe").output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["kind"], "rudder-execution-qualifier");
    assert_eq!(value["linux"], true);
    assert_eq!(value["default_launch_changed"], false);
}
fn sha(path: &Path) -> Vec<u8> {
    let mut file = fs::File::open(path).unwrap();
    let mut hash = Sha256::new();
    let mut bytes = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut bytes).unwrap();
        if n == 0 {
            break;
        }
        hash.update(&bytes[..n]);
    }
    hash.finalize().to_vec()
}
#[test]
fn qualifier_binary_comes_from_the_current_cargo_graph_without_namespace_effects() {
    describe(&qualifier());
}

#[test]
#[ignore = "explicit fixture artifact copy; actual execution belongs only on an owned qualified Linux worker"]
fn build_execution_driver() {
    let compiled = qualifier();
    describe(&compiled);
    let root = std::env::var_os("RUDDER_TEST_TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            if Path::new("/workspace").is_dir() {
                "/workspace".into()
            } else {
                std::env::temp_dir()
            }
        });
    let temporary = root.join(format!("rudder-execution-copy-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&temporary).unwrap();
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(temporary.clone());
    let output = std::env::var_os("RUDDER_EXECUTION_DRIVER_OUTPUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| temporary.join("controller"));
    assert_ne!(compiled, output, "cannot overwrite Cargo's input binary");
    fs::copy(&compiled, &output).unwrap();
    assert_eq!(sha(&compiled), sha(&output));
    describe(&output);
    std::io::stdout()
        .write_all(format!("execution_driver={}\n", output.display()).as_bytes())
        .unwrap();
}
