#![cfg(all(feature = "cli", target_os = "linux"))] // forks use Linux OverlayFS
//! rudder-jj behaves like stock jj on a normal repo, and forks via rudder-overlay.

use std::path::Path;
use std::process::Command;

fn jj(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_rudder-jj"))
        .args(args)
        .current_dir(cwd)
        .env("JJ_USER", "Test")
        .env("JJ_EMAIL", "test@invalid.example")
        .env("JJ_CONFIG", cwd.join("no-config.toml"))
        .env_remove("RUDDER_JJ_CLIENT_CONFIG")
        .env_remove("RUDDER_JJ_HOST_CONTEXT")
        .output()
        .unwrap();
    assert!(output.status.success(), "{args:?}: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn log_status_commit_on_a_normal_repo_and_overlay_fork_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    for i in 0..20 {
        std::fs::write(repo.join("src").join(format!("f{i}.txt")), format!("{i}\n")).unwrap();
    }
    jj(&repo, &["git", "init", "--colocate"]);
    assert!(jj(&repo, &["status"]).contains("A src/f1.txt"));
    jj(&repo, &["commit", "-m", "base"]);
    assert_eq!(jj(&repo, &["log", "--no-graph", "-r", "@-", "-T", "description"]), "base\n");
    assert!(jj(&repo, &["status"]).contains("The working copy has no changes"));

    // Fork with the main checkout itself as lower layer: no files copied.
    let fork = dir.path().join("fork");
    jj(&repo, &[
        "rudder-fork-add", fork.to_str().unwrap(), "--name", "f1", "-r", "@-",
        "--lower", repo.to_str().unwrap(), "--lowers-root", dir.path().join("lowers").to_str().unwrap(),
    ]);
    let names: Vec<_> = std::fs::read_dir(&fork).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert_eq!(names, vec![std::ffi::OsString::from(".jj")]);
    let upper = fork.join(".jj/working_copy/upper");
    std::fs::create_dir_all(upper.join("src")).unwrap();
    std::fs::write(upper.join("src/f3.txt"), "changed\n").unwrap();
    std::fs::write(upper.join("new.txt"), "new\n").unwrap();
    assert_eq!(jj(&fork, &["diff", "--summary"]), "A new.txt\nM src/f3.txt\n");
}
