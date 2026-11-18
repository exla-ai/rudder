//! Claude Code's suggested next prompt, for the Rudder app's composer.
//!
//! Claude Code guesses the person's next prompt after a turn and shows it dim
//! in its prompt box, but saves it nowhere: the session JSONL never has it. So
//! every Claude worker runs with a small Claude plugin (a `prompt.suggest` hook,
//! `native/claude-plugin/rudder-prompt-suggestion`) that writes the suggestion
//! the box shows to `<signals>/<run>-suggestion.json`, the path in
//! `RUDDER_PROMPT_SUGGESTION_FILE`, and `{"text": null}` once a turn starts.
//! The cloud worker serves it with the transcript (`cloud/worker/suggestion.mjs`).
//! Codex makes no such guess.
//!
//! `--plugin-dir` loads the plugin for that session only: Claude writes nothing
//! about it to the user's `~/.claude.json` or settings, and asks nothing.

use rudder_native::pty_terminal::TerminalCommand;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub(crate) const FILE_ENV: &str = "RUDDER_PROMPT_SUGGESTION_FILE";

const PLUGIN_DIR: &str = "rudder-prompt-suggestion";
const PLUGIN_FILES: [(&str, &str); 3] = [
    (
        ".claude-plugin/plugin.json",
        include_str!("../claude-plugin/rudder-prompt-suggestion/.claude-plugin/plugin.json"),
    ),
    (
        "hooks/hooks.json",
        include_str!("../claude-plugin/rudder-prompt-suggestion/hooks/hooks.json"),
    ),
    (
        "hooks/register.ts",
        include_str!("../claude-plugin/rudder-prompt-suggestion/hooks/register.ts"),
    ),
];

/// What one Claude launch needs: the plugin folder and the run's file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClaudeWiring {
    pub(crate) plugin_dir: PathBuf,
    pub(crate) file: PathBuf,
}

impl ClaudeWiring {
    /// Install the plugin in the signals folder and name the run's file.
    pub(crate) fn prepare(signals: &Path, run_id: &str) -> io::Result<Self> {
        Ok(Self {
            plugin_dir: install(signals)?,
            file: suggestion_file(signals, run_id),
        })
    }

    pub(crate) fn apply(&self, command: &mut TerminalCommand) {
        command.args.push("--plugin-dir".to_string());
        command.args.push(self.plugin_dir.display().to_string());
        command
            .env
            .push((FILE_ENV.to_string(), self.file.display().to_string()));
    }
}

/// One plugin folder serves every Claude worker. An interactive Claude session
/// hot-reloads a `--plugin-dir` folder, so a file is rewritten only when its
/// content changed, and through a rename so no session loads half a file.
/// (Claude lays its own type files in `.claude-plugin/types/`; they are left.)
pub(crate) fn install(signals: &Path) -> io::Result<PathBuf> {
    let root = signals.join(PLUGIN_DIR);
    for (relative, contents) in PLUGIN_FILES {
        let path = root.join(relative);
        if fs::read(&path).is_ok_and(|current| current == contents.as_bytes()) {
            continue;
        }
        let dir = path.parent().expect("every plugin file sits in a folder");
        fs::create_dir_all(dir)?;
        let name = path
            .file_name()
            .expect("every plugin file has a name")
            .to_string_lossy();
        let partial = dir.join(format!(".{name}.{}.partial", std::process::id()));
        fs::write(&partial, contents)?;
        fs::rename(&partial, &path)?;
    }
    Ok(root)
}

pub(crate) fn suggestion_file(signals: &Path, run_id: &str) -> PathBuf {
    signals.join(format!("{run_id}-suggestion.json"))
}

/// Remove a deleted run's suggestion file; one never written is fine.
pub(crate) fn cleanup(signals: &Path, run_id: &str) -> io::Result<()> {
    match fs::remove_file(suggestion_file(signals, run_id)) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
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
        let dir = std::env::temp_dir().join(format!(
            "rudder-prompt-suggestion-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    #[test]
    fn a_claude_launch_gets_the_plugin_and_its_runs_file() {
        let temp = tempdir();
        let signals = temp.0.join("signals");
        let wiring = ClaudeWiring::prepare(&signals, "run_a").unwrap();
        for (relative, contents) in PLUGIN_FILES {
            assert_eq!(
                fs::read_to_string(wiring.plugin_dir.join(relative)).unwrap(),
                contents
            );
        }

        // A stale copy is replaced; Claude's own files beside it stay.
        let register = wiring.plugin_dir.join("hooks/register.ts");
        fs::write(&register, "old").unwrap();
        let types = wiring
            .plugin_dir
            .join(".claude-plugin/types/claude-code/index.d.ts");
        fs::create_dir_all(types.parent().unwrap()).unwrap();
        fs::write(&types, "declare module 'claude-code' {}").unwrap();
        assert_eq!(install(&signals).unwrap(), wiring.plugin_dir);
        assert_eq!(fs::read_to_string(&register).unwrap(), PLUGIN_FILES[2].1);
        assert!(types.exists());
        let leftovers: Vec<_> = fs::read_dir(wiring.plugin_dir.join("hooks"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
            .filter(|name| name.ends_with(".partial"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");

        let mut command = TerminalCommand::with_args("claude", ["--settings", "s.json"]);
        wiring.apply(&mut command);
        let file = signals.join("run_a-suggestion.json");
        assert_eq!(
            command.args,
            [
                "--settings",
                "s.json",
                "--plugin-dir",
                wiring.plugin_dir.to_str().unwrap()
            ]
        );
        assert_eq!(
            command.env,
            [(FILE_ENV.to_string(), file.display().to_string())]
        );

        cleanup(&signals, "run_a").unwrap();
        fs::write(&file, r#"{"text":"run the tests","at":1}"#).unwrap();
        cleanup(&signals, "run_a").unwrap();
        assert!(!file.exists());
    }
}
