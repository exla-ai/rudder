//! Explicit standalone controller uses the actual TerminalPane spawn path.
#![cfg(target_os = "linux")]
const DRIVER: &str = include_str!("../src/execution/qualification_driver.rs");

#[test]
#[ignore = "explicit real TerminalPane/Fly qualification requires built native library"]
fn build_execution_driver() {
    use std::{fs, process::Command};
    let root = std::env::var_os("RUDDER_TEST_TMPDIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            if std::path::Path::new("/workspace").is_dir() {
                "/workspace".into()
            } else {
                std::env::temp_dir()
            }
        });
    let temp = tempfile::Builder::new()
        .prefix("rudder-execution-driver-")
        .tempdir_in(root)
        .unwrap();
    let source = temp.path().join("driver.rs");
    fs::write(&source, DRIVER).unwrap();
    let deps = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let lib = |name: &str| {
        fs::read_dir(&deps)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(&format!("lib{name}-"))
                    && e.path().extension().is_some_and(|e| e == "rlib")
            })
            .max_by_key(|e| e.metadata().unwrap().modified().unwrap())
            .unwrap()
            .path()
    };
    let output = std::env::var_os("RUDDER_EXECUTION_DRIVER_OUTPUT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| temp.path().join("driver"));
    let mut command = Command::new("rustc");
    command
        .arg("--edition=2021")
        .arg(source)
        .arg("-L")
        .arg(format!("dependency={}", deps.display()));
    for name in [
        "rudder_engine",
        "rudder_native",
        "serde_json",
        "uuid",
        "libc",
        "sha2",
        "hex",
    ] {
        command
            .arg("--extern")
            .arg(format!("{name}={}", lib(name).display()));
    }
    let result = command.arg("-o").arg(&output).output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    println!("execution_driver={}", output.display());
}
