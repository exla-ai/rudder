//! Claude Code on OpenRouter models: Kimi and GLM agents.
//!
//! A Kimi or GLM agent IS a Claude Code agent (`Backend::Claude`) whose model
//! is the alias `kimi` or `glm`. Hooks, signals, the subagent tree, compaction
//! status, resume and the `--settings` file all take the Claude path
//! unchanged; only that one process's environment differs. Two steps:
//!
//! - [`route_claude_command`], when a Claude command is built: `--model kimi`
//!   becomes the Claude Code model slot it rides on (`opus`, `sonnet` for
//!   GLM), and every slot names an OpenRouter model, so nothing in the
//!   session ever falls back to a real Claude model. No secrets yet.
//! - [`attach_credentials`], at PTY spawn, which every agent launch passes
//!   through (the managed `__claude-sdk` path too, which inherits the env):
//!   resolves the OpenRouter key and points the process at OpenRouter.
//!
//! The key travels only in the child's environment: never argv (visible in
//! `ps`), run records, RUDDER.md, logs or completion output. Nothing global
//! is written: main Claude agents and every other worker keep Anthropic.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};

use crate::pty_terminal::TerminalCommand;

/// The OpenRouter model ids. Updating a model is a one-line change here.
pub const KIMI_K3_1_PRO: &str = "moonshotai/kimi-k3.1-pro";
pub const GLM_5_3: &str = "z-ai/glm-5.3";
/// Plain Kimi K3.1 is not on OpenRouter (checked 2026-11-06: only K3.1 Pro
/// is). Once it is, put its id here and it takes the Fable slot.
pub const KIMI_K3_1: &str = "";

/// OpenRouter's Anthropic-compatible endpoint (Claude Code appends `/v1/...`).
pub const OPENROUTER_BASE_URL: &str = "https://openrouter.ai/api";

/// The one notice a Kimi or GLM launch gives when no key is found.
pub const MISSING_KEY: &str =
    "No OpenRouter key: run opencode auth login or set OPENROUTER_API_KEY";

/// Set by [`route_claude_command`] on a routed command; [`attach_credentials`]
/// acts only on commands that carry it.
pub const ROUTE_ENV: &str = "RUDDER_OPENROUTER_MODEL";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpenRouterModel {
    Kimi,
    Glm,
}

impl OpenRouterModel {
    pub const ALL: [Self; 2] = [Self::Kimi, Self::Glm];

    /// The model alias a run stores and the dashboard shows.
    pub fn alias(self) -> &'static str {
        match self {
            Self::Kimi => "kimi",
            Self::Glm => "glm",
        }
    }

    pub fn parse(model: &str) -> Option<Self> {
        let model = model.trim();
        Self::ALL
            .into_iter()
            .find(|candidate| candidate.alias().eq_ignore_ascii_case(model))
    }

    pub fn openrouter_id(self) -> &'static str {
        match self {
            Self::Kimi => KIMI_K3_1_PRO,
            Self::Glm => GLM_5_3,
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::Kimi => "Kimi K3.1 Pro",
            Self::Glm => "GLM 5.3",
        }
    }

    /// The Claude Code slot this model starts on. Passing the slot alias (not
    /// the raw id) keeps Claude Code's own `/model` picker marking the right row.
    pub fn claude_slot(self) -> &'static str {
        match self {
            Self::Kimi => "opus",
            Self::Glm => "sonnet",
        }
    }
}

/// Every Claude Code model slot, relabelled to an OpenRouter model. An unset
/// slot would fall back to the real Claude model (billed through OpenRouter),
/// so all of them are set, plus the small/fast model background tasks use.
///
///   Opus slot   -> Kimi K3.1 Pro
///   Fable slot  -> Kimi K3.1 once OpenRouter lists it; Kimi K3.1 Pro until then
///   Sonnet slot -> GLM 5.3
///   Haiku slot  -> GLM 5.3, the slot Claude Code runs its background tasks on
pub fn slot_env(model: OpenRouterModel) -> Vec<(String, String)> {
    let via = |id: &str| format!("{id} via OpenRouter");
    let (fable, fable_name, fable_desc) = if KIMI_K3_1.is_empty() {
        (
            KIMI_K3_1_PRO,
            "Kimi K3.1 Pro",
            "same as above; plain Kimi K3.1 isn't on OpenRouter yet".to_string(),
        )
    } else {
        (KIMI_K3_1, "Kimi K3.1", via(KIMI_K3_1))
    };
    [
        ("ANTHROPIC_DEFAULT_OPUS_MODEL", KIMI_K3_1_PRO.to_string()),
        ("ANTHROPIC_DEFAULT_OPUS_MODEL_NAME", "Kimi K3.1 Pro".to_string()),
        ("ANTHROPIC_DEFAULT_OPUS_MODEL_DESCRIPTION", via(KIMI_K3_1_PRO)),
        ("ANTHROPIC_DEFAULT_FABLE_MODEL", fable.to_string()),
        ("ANTHROPIC_DEFAULT_FABLE_MODEL_NAME", fable_name.to_string()),
        ("ANTHROPIC_DEFAULT_FABLE_MODEL_DESCRIPTION", fable_desc),
        ("ANTHROPIC_DEFAULT_SONNET_MODEL", GLM_5_3.to_string()),
        ("ANTHROPIC_DEFAULT_SONNET_MODEL_NAME", "GLM 5.3".to_string()),
        ("ANTHROPIC_DEFAULT_SONNET_MODEL_DESCRIPTION", via(GLM_5_3)),
        ("ANTHROPIC_DEFAULT_HAIKU_MODEL", GLM_5_3.to_string()),
        ("ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME", "GLM 5.3".to_string()),
        (
            "ANTHROPIC_DEFAULT_HAIKU_MODEL_DESCRIPTION",
            "same as above; Claude Code runs background tasks on this slot".to_string(),
        ),
        ("ANTHROPIC_SMALL_FAST_MODEL", GLM_5_3.to_string()),
        // `--model` on the command line already names the slot; this covers a
        // launch path that omits it, so the global default model never applies.
        ("ANTHROPIC_MODEL", model.claude_slot().to_string()),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_string(), value))
    .collect()
}

/// Route a Claude command whose `--model` is `kimi` or `glm` to OpenRouter.
/// Returns the model when it did; any other command is left untouched.
pub fn route_claude_command(command: &mut TerminalCommand) -> Option<OpenRouterModel> {
    let at = command.args.iter().position(|arg| arg == "--model")?;
    let model = OpenRouterModel::parse(command.args.get(at + 1)?)?;
    command.args[at + 1] = model.claude_slot().to_string();
    for (key, value) in slot_env(model) {
        set_env(command, &key, value);
    }
    set_env(command, ROUTE_ENV, model.alias().to_string());
    Some(model)
}

/// Point a routed command at OpenRouter with the resolved key. A command that
/// was not routed is left alone. Without a key the launch is refused with
/// [`MISSING_KEY`] rather than started against Anthropic.
pub fn attach_credentials(command: &mut TerminalCommand) -> Result<()> {
    attach_credentials_with(command, resolve_key)
}

pub fn attach_credentials_with(
    command: &mut TerminalCommand,
    resolve: impl FnOnce() -> Option<String>,
) -> Result<()> {
    if !command.env.iter().any(|(key, _)| key == ROUTE_ENV) {
        return Ok(());
    }
    let key = resolve().ok_or_else(|| anyhow!(MISSING_KEY))?;
    set_env(command, "ANTHROPIC_BASE_URL", OPENROUTER_BASE_URL.to_string());
    set_env(command, "ANTHROPIC_AUTH_TOKEN", key);
    // The token must be the only credential: an inherited Anthropic key or
    // Claude OAuth token would otherwise be sent to OpenRouter, or win.
    set_env(command, "ANTHROPIC_API_KEY", String::new());
    set_env(command, "CLAUDE_CODE_OAUTH_TOKEN", String::new());
    Ok(())
}

/// The OpenRouter key: `$OPENROUTER_API_KEY`, then opencode's login
/// (`opencode auth login` writes `~/.local/share/opencode/auth.json`), then an
/// `openrouter` api-key profile in Rudder's own `~/.rudder/auth-profiles.json`.
pub fn resolve_key() -> Option<String> {
    resolve_key_from(
        std::env::var("OPENROUTER_API_KEY").ok(),
        opencode_auth_path().as_deref(),
        rudder_auth_profiles_path().as_deref(),
    )
}

pub fn resolve_key_from(
    env: Option<String>,
    opencode_auth: Option<&Path>,
    rudder_profiles: Option<&Path>,
) -> Option<String> {
    let nonempty = |value: &str| {
        let value = value.trim();
        (!value.is_empty()).then(|| value.to_string())
    };
    if let Some(key) = env.as_deref().and_then(nonempty) {
        return Some(key);
    }
    let read = |path: Option<&Path>| -> Option<serde_json::Value> {
        serde_json::from_str(&std::fs::read_to_string(path?).ok()?).ok()
    };
    if let Some(key) = read(opencode_auth)
        .as_ref()
        .and_then(|auth| auth.pointer("/openrouter/key")?.as_str())
        .and_then(nonempty)
    {
        return Some(key);
    }
    let store = read(rudder_profiles)?;
    let profiles = store.get("profiles")?.as_object()?;
    let api_key = |profile: &serde_json::Value| {
        (profile.get("type")?.as_str()? == "api_key"
            && profile.get("provider")?.as_str()? == "openrouter")
            .then(|| profile.get("key")?.as_str().and_then(nonempty))
            .flatten()
    };
    profiles
        .get("openrouter:default")
        .and_then(api_key)
        .or_else(|| profiles.values().find_map(api_key))
}

fn opencode_auth_path() -> Option<PathBuf> {
    let data = std::env::var_os("XDG_DATA_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|home| home.join(".local").join("share")))?;
    Some(data.join("opencode").join("auth.json"))
}

fn rudder_auth_profiles_path() -> Option<PathBuf> {
    let home = std::env::var_os("RUDDER_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|home| home.join(".rudder")))?;
    Some(home.join("auth-profiles.json"))
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn set_env(command: &mut TerminalCommand, key: &str, value: String) {
    command.env.retain(|(existing, _)| existing != key);
    command.env.push((key.to_string(), value));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(command: &'a TerminalCommand, key: &str) -> Option<&'a str> {
        command
            .env
            .iter()
            .rev()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }

    #[test]
    fn kimi_routes_to_the_opus_slot_and_every_slot_names_an_openrouter_model() {
        let mut command =
            TerminalCommand::with_args("claude", ["--permission-mode", "bypassPermissions", "--model", "kimi", "task"]);
        assert_eq!(route_claude_command(&mut command), Some(OpenRouterModel::Kimi));
        assert_eq!(command.args, ["--permission-mode", "bypassPermissions", "--model", "opus", "task"]);
        assert_eq!(env(&command, ROUTE_ENV), Some("kimi"));
        assert_eq!(env(&command, "ANTHROPIC_MODEL"), Some("opus"));
        for slot in ["OPUS", "FABLE", "SONNET", "HAIKU"] {
            let id = env(&command, &format!("ANTHROPIC_DEFAULT_{slot}_MODEL")).unwrap();
            assert!(id == KIMI_K3_1_PRO || id == GLM_5_3, "{slot} -> {id}");
        }
        assert_eq!(env(&command, "ANTHROPIC_SMALL_FAST_MODEL"), Some(GLM_5_3));
        // Routing carries no credentials; those come at spawn.
        assert_eq!(env(&command, "ANTHROPIC_BASE_URL"), None);
        assert_eq!(env(&command, "ANTHROPIC_AUTH_TOKEN"), None);

        let mut glm = TerminalCommand::with_args("claude", ["--model", "GLM"]);
        assert_eq!(route_claude_command(&mut glm), Some(OpenRouterModel::Glm));
        assert_eq!(glm.args, ["--model", "sonnet"]);
    }

    #[test]
    fn claude_models_are_not_routed_and_get_no_credentials() {
        for model in ["opus", "sonnet", "claude-sonnet-5", "moonshotai/kimi-k3.1-pro"] {
            let mut command = TerminalCommand::with_args("claude", ["--model", model]);
            assert_eq!(route_claude_command(&mut command), None, "{model}");
            attach_credentials_with(&mut command, || panic!("no key lookup for {model}")).unwrap();
            assert!(command.env.is_empty(), "{model}: {:?}", command.env);
            assert_eq!(command.args, ["--model", model]);
        }
    }

    #[test]
    fn credentials_go_only_into_the_child_env_and_a_missing_key_refuses() {
        let key = "sk-or-test-0123456789";
        let mut command = TerminalCommand::with_args("claude", ["--model", "glm", "say hi"]);
        route_claude_command(&mut command);
        attach_credentials_with(&mut command, || Some(key.to_string())).unwrap();
        assert_eq!(env(&command, "ANTHROPIC_BASE_URL"), Some(OPENROUTER_BASE_URL));
        assert_eq!(env(&command, "ANTHROPIC_AUTH_TOKEN"), Some(key));
        assert_eq!(env(&command, "ANTHROPIC_API_KEY"), Some(""));
        assert_eq!(env(&command, "CLAUDE_CODE_OAUTH_TOKEN"), Some(""));
        assert!(command.args.iter().all(|arg| !arg.contains(key)));
        assert!(!command.program.contains(key));

        let mut keyless = TerminalCommand::with_args("claude", ["--model", "kimi"]);
        route_claude_command(&mut keyless);
        let error = attach_credentials_with(&mut keyless, || None).unwrap_err();
        assert_eq!(error.to_string(), MISSING_KEY);
        assert_eq!(env(&keyless, "ANTHROPIC_BASE_URL"), None);
    }

    #[test]
    fn key_comes_from_env_then_opencode_then_rudder_profiles() {
        let dir = std::env::temp_dir().join(format!("rudder-openrouter-key-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let opencode = dir.join("auth.json");
        let profiles = dir.join("auth-profiles.json");
        std::fs::write(&opencode, r#"{"baseten":{"type":"api","key":"b"},"openrouter":{"type":"api","key":"from-opencode"}}"#).unwrap();
        std::fs::write(&profiles, r#"{"version":1,"profiles":{"anthropic:x":{"type":"api_key","provider":"anthropic","key":"no"},"or":{"type":"api_key","provider":"openrouter","key":"from-rudder"}}}"#).unwrap();

        assert_eq!(resolve_key_from(Some(" from-env ".into()), Some(&opencode), Some(&profiles)).as_deref(), Some("from-env"));
        assert_eq!(resolve_key_from(Some("  ".into()), Some(&opencode), Some(&profiles)).as_deref(), Some("from-opencode"));
        assert_eq!(resolve_key_from(None, Some(&dir.join("missing.json")), Some(&profiles)).as_deref(), Some("from-rudder"));
        std::fs::write(&opencode, r#"{"baseten":{"type":"api","key":"b"}}"#).unwrap();
        assert_eq!(resolve_key_from(None, Some(&opencode), None), None);

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
