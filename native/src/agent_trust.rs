//! Pre-accepts the folder-trust prompt Claude Code and Codex show the first
//! time they run in a folder ("Yes, I trust this folder", "Trust and
//! continue"). Every Rudder agent starts in a fresh workspace, so without this
//! each one stopped on that prompt until someone answered it, and on a Cloud
//! home the answer was lost again whenever the laptop's `~/.codex/config.toml`
//! was copied back over the saved one. Rudder launches the agent in that folder
//! itself, so it records the same decision the user would have made, once per
//! repository, before every launch.
//!
//! Both CLIs key trust by the Git repository root (a Rudder workspace inside a
//! repo inherits the repo's decision), so the root is what gets trusted. An
//! explicit `trust_level` the user already chose in Codex is never changed, and
//! a file this code cannot read safely is left alone.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Best effort, before the agent's PTY starts: a failure here only means the
/// agent shows its usual prompt, which the dashboard already flags.
pub(crate) fn prepare_for_launch(program: &str, cwd: &Path) {
    let name = Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    // A RUDDER_*_BIN override is a test fake or a custom build: leave the real
    // config files alone.
    let config = match name {
        "claude" if std::env::var_os("RUDDER_CLAUDE_BIN").is_none() => claude_config_path(),
        "codex" if std::env::var_os("RUDDER_CODEX_BIN").is_none() => codex_config_path(),
        _ => return,
    };
    if let Some(config) = config {
        trust_for(name, &config, cwd);
    }
}

/// Records trust for `cwd`'s roots in `config` for the CLI named `name`. An
/// overlay fork (src/warm-snapshot.ts) is trusted at its workspace path, which
/// is also the agent's cwd inside the fork's namespace.
pub(crate) fn trust_for(name: &str, config: &Path, cwd: &Path) {
    let trust: fn(&Path, &str) -> std::io::Result<bool> = match name {
        "claude" => trust_claude,
        "codex" => trust_codex,
        _ => return,
    };
    for root in trust_roots(cwd) {
        if let Some(root) = root.to_str() {
            let _ = trust(config, root);
        }
    }
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
}

fn claude_config_path() -> Option<PathBuf> {
    match std::env::var_os("CLAUDE_CONFIG_DIR").filter(|dir| !dir.is_empty()) {
        Some(dir) => Some(PathBuf::from(dir).join(".claude.json")),
        None => home().map(|home| home.join(".claude.json")),
    }
}

fn codex_config_path() -> Option<PathBuf> {
    match std::env::var_os("CODEX_HOME").filter(|dir| !dir.is_empty()) {
        Some(dir) => Some(PathBuf::from(dir).join("config.toml")),
        None => home().map(|home| home.join(".codex").join("config.toml")),
    }
}

/// The repository root the CLIs record trust for: the nearest ancestor with a
/// `.git` (a jj workspace inside the user's checkout resolves to the checkout),
/// plus the main checkout of a Git worktree. Outside a repository, the folder.
pub(crate) fn trust_roots(cwd: &Path) -> Vec<PathBuf> {
    let cwd = fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let Some(root) = cwd.ancestors().find(|dir| dir.join(".git").exists()) else {
        return vec![cwd];
    };
    let mut roots = vec![root.to_path_buf()];
    if let Ok(pointer) = fs::read_to_string(root.join(".git")) {
        if let Some(gitdir) = pointer.trim().strip_prefix("gitdir:") {
            if let Some((main, _)) = gitdir.trim().split_once("/.git/worktrees/") {
                let main = PathBuf::from(main);
                if main.is_absolute() && !roots.contains(&main) {
                    roots.push(main);
                }
            }
        }
    }
    roots
}

/// Sets `projects[root].hasTrustDialogAccepted` in `.claude.json`. Returns
/// whether the file changed. A missing file means Claude was never set up
/// here; its own first run creates it.
pub(crate) fn trust_claude(config: &Path, root: &str) -> std::io::Result<bool> {
    retry_if_changed(|| trust_claude_once(config, root))
}

fn trust_claude_once(config: &Path, root: &str) -> std::io::Result<Outcome> {
    let text = match fs::read_to_string(config) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Outcome::Unchanged),
        Err(error) => return Err(error),
    };
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Ok(Outcome::Unchanged);
    };
    let Some(top) = value.as_object_mut() else {
        return Ok(Outcome::Unchanged);
    };
    let projects = top
        .entry("projects")
        .or_insert_with(|| serde_json::Value::Object(Default::default()));
    let Some(projects) = projects.as_object_mut() else {
        return Ok(Outcome::Unchanged);
    };
    let project = projects
        .entry(root.to_string())
        .or_insert_with(|| serde_json::Value::Object(Default::default()));
    let Some(project) = project.as_object_mut() else {
        return Ok(Outcome::Unchanged);
    };
    // Trust, plus the one-time "Allow external CLAUDE.md file imports?" prompt.
    // Workers run in a workspace below the checkout, so the checkout's
    // CLAUDE.md (and anything it imports, such as AGENTS.md) sits outside the
    // worker's directory and Claude asks about it in every new worker.
    const KEYS: [&str; 3] = [
        "hasTrustDialogAccepted",
        "hasClaudeMdExternalIncludesApproved",
        "hasClaudeMdExternalIncludesWarningShown",
    ];
    let accepted = serde_json::Value::Bool(true);
    if KEYS.iter().all(|key| project.get(*key) == Some(&accepted)) {
        return Ok(Outcome::Unchanged);
    }
    for key in KEYS {
        project.insert(key.into(), accepted.clone());
    }
    let mut out = serde_json::to_string_pretty(&value).map_err(std::io::Error::other)?;
    out.push('\n');
    replace_file(config, &text, out.as_bytes())
}

/// Adds `[projects."<root>"] trust_level = "trusted"` to Codex's
/// `config.toml`. Returns whether the file changed.
pub(crate) fn trust_codex(config: &Path, root: &str) -> std::io::Result<bool> {
    retry_if_changed(|| trust_codex_once(config, root))
}

fn trust_codex_once(config: &Path, root: &str) -> std::io::Result<Outcome> {
    let text = match fs::read_to_string(config) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if !config.parent().is_some_and(Path::is_dir) {
                return Ok(Outcome::Unchanged);
            }
            String::new()
        }
        Err(error) => return Err(error),
    };
    let Some(out) = codex_config_with_trust(&text, root) else {
        return Ok(Outcome::Unchanged);
    };
    replace_file(config, &text, out.as_bytes())
}

/// The edited text, or None when nothing should change: the root already has
/// a `trust_level` (trusted or the user's explicit choice), or it appears in a
/// form this line-level edit does not recognize (never risk a duplicate table,
/// which would make Codex refuse the whole file).
pub(crate) fn codex_config_with_trust(text: &str, root: &str) -> Option<String> {
    let key = toml_basic_string(root);
    let header = format!("[projects.{key}]");
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    if let Some(at) = lines.iter().position(|line| line.trim() == header) {
        for line in &lines[at + 1..] {
            let line = line.trim();
            if line.starts_with('[') {
                break;
            }
            if line
                .strip_prefix("trust_level")
                .is_some_and(|rest| rest.trim_start().starts_with('='))
            {
                return None;
            }
        }
        lines.insert(at + 1, "trust_level = \"trusted\"".into());
        let mut out = lines.join("\n");
        out.push('\n');
        return Some(out);
    }
    let literal = format!("'{root}'");
    if lines
        .iter()
        .any(|line| line.contains("projects") && (line.contains(&key) || line.contains(&literal)))
    {
        return None;
    }
    let mut out = text.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(&format!("{header}\ntrust_level = \"trusted\"\n"));
    Some(out)
}

fn toml_basic_string(value: &str) -> String {
    let mut out = String::from("\"");
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            ch if ch.is_control() => out.push_str(&format!("\\u{:04X}", ch as u32)),
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}

enum Outcome {
    Unchanged,
    Written,
    /// Another process rewrote the file after it was read; read it again.
    Raced,
}

/// Claude and Codex rewrite their whole config file at any time. An edit is
/// applied to what was read, so if the file changed before the rename it is
/// redone from the new contents (a few times, then left for the next launch).
fn retry_if_changed(mut attempt: impl FnMut() -> std::io::Result<Outcome>) -> std::io::Result<bool> {
    for _ in 0..3 {
        match attempt()? {
            Outcome::Unchanged => return Ok(false),
            Outcome::Written => return Ok(true),
            Outcome::Raced => continue,
        }
    }
    Ok(false)
}

/// Atomic replace next to the real file: a symlinked config (the Cloud home
/// links `~/.codex/config.toml` into its volume) keeps its link, and the file
/// keeps its mode. Just before the rename the file is read again; if it no
/// longer holds `original`, nothing is replaced and the caller retries.
fn replace_file(config: &Path, original: &str, bytes: &[u8]) -> std::io::Result<Outcome> {
    let target = fs::canonicalize(config).unwrap_or_else(|_| config.to_path_buf());
    let mode = fs::metadata(&target)
        .map(|meta| meta.permissions().mode() & 0o7777)
        .unwrap_or(0o600);
    let name = target
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config");
    let temp = target.with_file_name(format!(".{name}.rudder-trust-{}.tmp", std::process::id()));
    let written = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::set_permissions(&temp, fs::Permissions::from_mode(mode))?;
        let current = match fs::read_to_string(&target) {
            Ok(current) => current,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(error),
        };
        if current != original {
            return Ok(Outcome::Raced);
        }
        fs::rename(&temp, &target)?;
        Ok(Outcome::Written)
    })();
    if !matches!(written, Ok(Outcome::Written)) {
        let _ = fs::remove_file(&temp);
    }
    written
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A temp dir removed on drop (this crate has no tempfile dependency).
    struct TempDir(PathBuf);
    impl TempDir {
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn tempdir() -> TempDir {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "rudder-agent-trust-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    #[test]
    fn codex_trust_is_appended_once_and_keeps_user_choices() {
        let root = "/work/repo";
        let added = codex_config_with_trust("model = \"gpt\"\n", root).unwrap();
        assert_eq!(
            added,
            "model = \"gpt\"\n\n[projects.\"/work/repo\"]\ntrust_level = \"trusted\"\n"
        );
        assert_eq!(codex_config_with_trust(&added, root), None, "already trusted");
        let untrusted = "[projects.\"/work/repo\"]\ntrust_level = \"untrusted\"\n";
        assert_eq!(codex_config_with_trust(untrusted, root), None, "the user's choice stays");
        let bare = "[projects.\"/work/repo\"]\n\n[other]\nx = 1\n";
        assert_eq!(
            codex_config_with_trust(bare, root).unwrap(),
            "[projects.\"/work/repo\"]\ntrust_level = \"trusted\"\n\n[other]\nx = 1\n"
        );
        let literal = "[projects.'/work/repo']\ntrust_level = \"untrusted\"\n";
        assert_eq!(codex_config_with_trust(literal, root), None, "never a duplicate table");
        assert_eq!(
            codex_config_with_trust("", "/a\"b\\c").unwrap(),
            "[projects.\"/a\\\"b\\\\c\"]\ntrust_level = \"trusted\"\n"
        );
        // Other projects' tables are not the root's.
        let other = "[projects.\"/work/repo2\"]\ntrust_level = \"trusted\"\n";
        assert!(codex_config_with_trust(other, root).unwrap().ends_with(
            "[projects.\"/work/repo\"]\ntrust_level = \"trusted\"\n"
        ));
    }

    #[test]
    fn files_are_replaced_in_place_through_symlinks_with_their_mode() {
        let temp = tempdir();
        let real = temp.path().join("volume");
        fs::create_dir(&real).unwrap();
        fs::write(real.join("config.toml"), "model = \"gpt\"\n").unwrap();
        fs::set_permissions(real.join("config.toml"), fs::Permissions::from_mode(0o600)).unwrap();
        let link = temp.path().join("config.toml");
        std::os::unix::fs::symlink(real.join("config.toml"), &link).unwrap();
        assert!(trust_codex(&link, "/work/repo").unwrap());
        assert!(!trust_codex(&link, "/work/repo").unwrap(), "idempotent");
        assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        let text = fs::read_to_string(real.join("config.toml")).unwrap();
        assert!(text.contains("[projects.\"/work/repo\"]\ntrust_level = \"trusted\""));
        assert_eq!(
            fs::metadata(real.join("config.toml")).unwrap().permissions().mode() & 0o777,
            0o600
        );

        let claude = temp.path().join(".claude.json");
        assert!(!trust_claude(&claude, "/work/repo").unwrap(), "no file: Claude not set up");
        fs::write(
            &claude,
            r#"{"numStartups":3,"projects":{"/work/repo":{"allowedTools":["Bash"]}}}"#,
        )
        .unwrap();
        assert!(trust_claude(&claude, "/work/repo").unwrap());
        assert!(!trust_claude(&claude, "/work/repo").unwrap(), "idempotent");
        let value: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&claude).unwrap()).unwrap();
        assert_eq!(value["numStartups"], 3);
        assert_eq!(value["projects"]["/work/repo"]["allowedTools"][0], "Bash");
        assert_eq!(value["projects"]["/work/repo"]["hasTrustDialogAccepted"], true);
        // Workers sit below the checkout, so its CLAUDE.md imports count as
        // external: approve them once instead of asking in every worker.
        assert_eq!(value["projects"]["/work/repo"]["hasClaudeMdExternalIncludesApproved"], true);
        assert_eq!(value["projects"]["/work/repo"]["hasClaudeMdExternalIncludesWarningShown"], true);
        // A repo trusted before this change still gets the import approval.
        fs::write(
            &claude,
            r#"{"projects":{"/work/repo":{"hasTrustDialogAccepted":true}}}"#,
        )
        .unwrap();
        assert!(trust_claude(&claude, "/work/repo").unwrap());
        let value: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&claude).unwrap()).unwrap();
        assert_eq!(value["projects"]["/work/repo"]["hasClaudeMdExternalIncludesApproved"], true);
        fs::write(&claude, "not json").unwrap();
        assert!(!trust_claude(&claude, "/work/repo").unwrap(), "unreadable file left alone");
        assert_eq!(fs::read_to_string(&claude).unwrap(), "not json");
    }

    #[test]
    fn a_file_rewritten_after_the_read_is_not_replaced() {
        let temp = tempdir();
        let config = temp.path().join("config.toml");
        fs::write(&config, "written by codex meanwhile\n").unwrap();
        let raced = replace_file(&config, "what was read\n", b"edited\n").unwrap();
        assert!(matches!(raced, Outcome::Raced));
        assert_eq!(fs::read_to_string(&config).unwrap(), "written by codex meanwhile\n");
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 1, "no temp file left");
        // The public path re-reads and applies the edit to the new contents.
        assert!(trust_codex(&config, "/work/repo").unwrap());
        assert!(fs::read_to_string(&config)
            .unwrap()
            .starts_with("written by codex meanwhile\n\n[projects.\"/work/repo\"]"));
    }

    #[test]
    fn fork_launches_record_trust_for_claude_and_codex() {
        let temp = tempdir();
        let base = fs::canonicalize(temp.path()).unwrap();
        // A fork outside any checkout: only `.jj` on disk, plus `<path>.fork/`.
        let fork = base.join("ws/agent-1");
        fs::create_dir_all(fork.join(".jj")).unwrap();
        fs::create_dir_all(base.join("ws/agent-1.fork")).unwrap();
        let claude = base.join(".claude.json");
        fs::write(&claude, "{}").unwrap();
        let codex = base.join("config.toml");
        trust_for("claude", &claude, &fork);
        trust_for("codex", &codex, &fork);
        let root = fork.to_str().unwrap();
        let value: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&claude).unwrap()).unwrap();
        assert_eq!(value["projects"][root]["hasTrustDialogAccepted"], true);
        assert!(fs::read_to_string(&codex)
            .unwrap()
            .contains(&format!("[projects.\"{root}\"]")));
    }

    #[test]
    fn workspaces_inside_a_checkout_trust_the_checkout() {
        let temp = tempdir();
        let repo = fs::canonicalize(temp.path()).unwrap().join("repo");
        let workspace = repo.join(".rudder-workspaces/project/agent-1");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::create_dir_all(workspace.join(".jj")).unwrap();
        assert_eq!(trust_roots(&workspace), vec![repo.clone()]);
        let worktree = temp.path().join("wt");
        fs::create_dir_all(&worktree).unwrap();
        fs::write(
            worktree.join(".git"),
            format!("gitdir: {}/.git/worktrees/wt\n", repo.display()),
        )
        .unwrap();
        let worktree = fs::canonicalize(&worktree).unwrap();
        assert_eq!(trust_roots(&worktree), vec![worktree.clone(), repo]);
        let plain = fs::canonicalize(temp.path()).unwrap().join("plain");
        fs::create_dir_all(&plain).unwrap();
        assert_eq!(trust_roots(&plain), vec![plain]);
    }
}
