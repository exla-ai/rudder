//! Claude Code loads CLAUDE.md from every directory above its cwd, and an agent
//! workspace sits inside the main checkout (`<root>/.rudder-workspaces/<group>/<name>`).
//! So each worker loaded the project's memory twice: the main checkout's copy
//! and its own. In libra, whose CLAUDE.md imports a 300k-character AGENTS.md,
//! that took a worker's first turn from about 131k to 249k tokens. The outer
//! copies go into the worker's own `--settings` file as `claudeMdExcludes`.

use std::fs;
use std::path::{Path, PathBuf};

/// Both spellings of the agent-workspaces dir (`AGENT_WORKSPACES_DIR` and
/// `LEGACY_AGENT_WORKSPACES_DIR` in gitio.rs).
const WORKSPACES_DIRS: [&str; 2] = [".rudder-workspaces", ".rudder-worktrees"];

/// Claude matches excludes as globs, so a path with glob syntax could exclude
/// something else. Such a file stays loaded.
const GLOB_CHARS: [char; 10] = ['*', '?', '[', ']', '{', '}', '(', ')', '!', '\\'];

/// Best effort, before the agent's PTY starts: on any failure the worker loads
/// both copies, as before. Only a `--settings` FILE is edited (Rudder writes one
/// per worker run); main agents run in the checkout itself and are left alone.
pub(crate) fn exclude_outer_memory(args: &[String], cwd: &Path) {
    let Some(settings) = args
        .iter()
        .rposition(|arg| arg == "--settings")
        .and_then(|at| args.get(at + 1))
        .map(Path::new)
    else {
        return;
    };
    let excludes = outer_memory(cwd);
    if excludes.is_empty() {
        return;
    }
    let Some(mut json) = fs::read_to_string(settings)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
    else {
        return;
    };
    let Some(object) = json.as_object_mut() else {
        return;
    };
    object.insert("claudeMdExcludes".to_string(), excludes.into());
    let _ = fs::write(settings, json.to_string());
}

/// The main checkout's memory files that the workspace at `cwd` has its own
/// copy of. Excluding a file also drops what it imports (`@AGENTS.md`). Files
/// only the main checkout has, such as an ignored CLAUDE.local.md or .claude/,
/// still load.
pub(crate) fn outer_memory(cwd: &Path) -> Vec<String> {
    let Some(root) = main_checkout(cwd) else {
        return Vec::new();
    };
    // An overlay fork's files exist only inside its mount namespace. Outside it
    // they are the warm snapshot's, named in `<workspace>.fork/lower`.
    let mut state = cwd.as_os_str().to_owned();
    state.push(".fork");
    let lower = fs::read_to_string(PathBuf::from(state).join("lower"))
        .ok()
        .map(|dir| PathBuf::from(dir.trim()));
    let mut names: Vec<PathBuf> = ["CLAUDE.md", "CLAUDE.local.md", ".claude/CLAUDE.md"]
        .into_iter()
        .map(PathBuf::from)
        .collect();
    markdown_under(&root, Path::new(".claude/rules"), &mut names);
    let mut excludes: Vec<String> = Vec::new();
    for name in names {
        let outer = root.join(&name);
        let copied = cwd.join(&name).is_file()
            || lower.as_ref().is_some_and(|lower| lower.join(&name).is_file());
        if !outer.is_file() || !copied {
            continue;
        }
        // Claude may report the resolved path (macOS /tmp is /private/tmp).
        let resolved = fs::canonicalize(&outer).unwrap_or_else(|_| outer.clone());
        for path in [outer, resolved] {
            if let Some(path) = path.to_str() {
                if !path.contains(GLOB_CHARS) && !excludes.iter().any(|seen| seen == path) {
                    excludes.push(path.to_string());
                }
            }
        }
    }
    excludes
}

/// The directory holding the agent-workspaces dir that `cwd` is inside.
fn main_checkout(cwd: &Path) -> Option<PathBuf> {
    let mut root = PathBuf::new();
    for component in cwd.components() {
        if WORKSPACES_DIRS.iter().any(|dir| component.as_os_str() == *dir) {
            return root.is_absolute().then_some(root);
        }
        root.push(component);
    }
    None
}

/// Every `.md` under `root/dir`, as paths relative to `root`. Symlinked
/// directories are not followed.
fn markdown_under(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(root.join(dir)) else {
        return;
    };
    for entry in entries.flatten() {
        let name = dir.join(entry.file_name());
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => markdown_under(root, &name, out),
            Ok(_) if name.extension().is_some_and(|ext| ext == "md") => out.push(name),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn tempdir() -> TempDir {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "rudder-claude-memory-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        TempDir(fs::canonicalize(&dir).unwrap())
    }

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn path(path: PathBuf) -> String {
        path.to_str().unwrap().to_string()
    }

    #[test]
    fn a_workspace_skips_the_main_checkouts_copy_of_its_memory() {
        let temp = tempdir();
        let root = temp.0.join("repo");
        let workspace = root.join(".rudder-workspaces/repo-1a2b/agent-1");
        for name in ["CLAUDE.md", "AGENTS.md", ".claude/rules/style.md"] {
            write(&root.join(name), "x");
            write(&workspace.join(name), "x");
        }
        // Ignored, so only the main checkout has them: the worker still reads them.
        write(&root.join("CLAUDE.local.md"), "mine");
        write(&root.join(".claude/rules/local/notes.md"), "mine");
        assert_eq!(
            outer_memory(&workspace),
            vec![path(root.join("CLAUDE.md")), path(root.join(".claude/rules/style.md"))]
        );
        // The main agent runs in the checkout itself.
        assert!(outer_memory(&root).is_empty());
    }

    #[test]
    fn an_overlay_fork_has_the_warm_snapshots_files() {
        let temp = tempdir();
        let root = temp.0.join("repo");
        let group = root.join(".rudder-workspaces/repo-1a2b");
        let fork = group.join("agent-1");
        let snapshot = group.join(".warm/9f2c");
        write(&root.join("CLAUDE.md"), "x");
        write(&snapshot.join("CLAUDE.md"), "x");
        fs::create_dir_all(fork.join(".jj")).unwrap();
        write(&group.join("agent-1.fork/lower"), &path(snapshot.clone()));
        assert_eq!(outer_memory(&fork), vec![path(root.join("CLAUDE.md"))]);
    }

    #[test]
    fn the_excludes_join_the_workers_settings_file() {
        let temp = tempdir();
        let root = temp.0.join("repo");
        let workspace = root.join(".rudder-workspaces/repo-1a2b/agent-1");
        write(&root.join("CLAUDE.md"), "x");
        write(&workspace.join("CLAUDE.md"), "x");
        let settings = temp.0.join("run-claude.json");
        write(&settings, r#"{"hooks":{"Stop":[]}}"#);
        let args = |settings: &str| -> Vec<String> {
            ["--model", "opus", "--settings", settings, "do the task"].map(String::from).to_vec()
        };
        exclude_outer_memory(&args(&path(settings.clone())), &workspace);
        let value: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&settings).unwrap()).unwrap();
        assert_eq!(value["claudeMdExcludes"], serde_json::json!([path(root.join("CLAUDE.md"))]));
        assert_eq!(value["hooks"], serde_json::json!({"Stop": []}), "the hooks stay");
        // Inline JSON settings, no settings, or a checkout that is not a workspace: unchanged.
        exclude_outer_memory(&args(r#"{"hooks":{}}"#), &workspace);
        exclude_outer_memory(&["--model".to_string(), "opus".to_string()], &workspace);
        write(&settings, "{}");
        exclude_outer_memory(&args(&path(settings.clone())), &root);
        assert_eq!(fs::read_to_string(&settings).unwrap(), "{}");
    }

    #[test]
    fn paths_with_glob_syntax_stay_loaded() {
        let temp = tempdir();
        let root = temp.0.join("repo (2)");
        let workspace = root.join(".rudder-workspaces/repo-1a2b/agent-1");
        write(&root.join("CLAUDE.md"), "x");
        write(&workspace.join("CLAUDE.md"), "x");
        assert!(outer_memory(&workspace).is_empty());
    }
}
