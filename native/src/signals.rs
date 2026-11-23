//! Official, backend-native completion signals.
//!
//! Instead of SCRAPING a worker's PTY chrome to GUESS when a turn ended (the
//! fragile string heuristics in `detect.rs`), we wire each backend's OWN
//! lifecycle hooks to write a tiny signal file that the poll loop reads as the
//! AUTHORITATIVE done/idle source. Rudder never guesses completion from terminal chrome.
//!
//! - **Claude Code** (`code.claude.com/docs/en/hooks`): a `Stop` hook fires when
//!   the turn ends; a `Notification` hook with matcher `idle_prompt` fires when it
//!   pauses for input. Wired via `claude --settings <file>`.
//! - **Codex** (`developers.openai.com/codex`): the `notify` program fires
//!   `agent-turn-complete`. Wired via `-c notify=[<script>]`. (Codex's newer
//!   `[hooks]` are trust-gated, so unusable for a headless child; `notify` needs
//!   no trust and is the right fit.)
//!
//! - **opencode** (`opencode.ai/docs/plugins`): Rudder's plugin's `event` hook
//!   sees `session.status`, `session.idle`, `session.error`, `permission.asked` /
//!   `permission.replied` and `question.asked` / `question.replied`.
//!
//! - **Antigravity** (`agy`, its built-in `agy-customizations` skill,
//!   `docs/hooks.md`): `hooks.json` lifecycle hooks. Antigravity reads hooks
//!   only from `~/.gemini/config/hooks.json` and `<workspace>/.agents/hooks.json`
//!   (no launch flag, and `--settings` ignores a `hooks` key), so Rudder keeps
//!   one named `rudder` entry in the global file whose commands do nothing
//!   unless `RUDDER_AGY_HOOK` names a run's script, which only Rudder's own
//!   launches set. `SessionStart` reports the conversation id, `PreInvocation`
//!   the start of a turn and `Stop` its end.
//!
//! All write `<rudder_home>/signals/<run_id>.json` =
//! `{"state":"done"|"input"|"permission"|"working"|"error"|"compacted","detail":"..."}`.
//! `detail` is optional and only carried by `error` (the backend's error type).

#![allow(unused_imports)]
use super::*;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SignalState {
    /// The worker finished its turn (Claude `Stop` / Codex `agent-turn-complete`
    /// / opencode root-session `session.idle`).
    Done,
    /// The worker paused with a question (Claude `Notification` matcher
    /// `idle_prompt`).
    Input,
    /// The worker is blocked on an approval prompt (Claude `Notification`
    /// matcher `permission_request` / opencode `permission.updated`). Distinct
    /// from `Input`: a question needs an answer, a permission needs a decision,
    /// and the dashboard colours them differently.
    Permission,
    /// The human answered and the worker went back to work (Claude
    /// `UserPromptSubmit` / opencode `permission.replied`).
    ///
    /// This is the counterpart the protocol was missing. `Input` and
    /// `Permission` latch — they have to, or a row would drop its "needs you"
    /// badge the moment the prompt scrolled out of the visible pane — and
    /// nothing ever lifted the latch, so answering an agent left it flagged
    /// until its NEXT turn ended.
    Working,
    /// The turn ENDED, but on an error rather than a reply (Claude `StopFailure`:
    /// rate limit, auth, overloaded, billing...; opencode `session.error`). The
    /// process is still alive and idle, so process exit never reports it, and
    /// `Stop` does not fire for it either — a row hit by a rate limit used to
    /// spin as "running" until you happened to look at the pane.
    Failed,
    /// A manual `/compact` finished (Claude and Codex `PostCompact` with
    /// `trigger: manual`). The agent is back at its prompt and no turn is
    /// running. A manual compaction runs between turns and fires neither
    /// `UserPromptSubmit` nor `Stop` (measured on Claude 2.1.318 and Codex
    /// 0.166), so a row that compaction had flipped to Running stayed "working"
    /// with nothing left to end it. An automatic compaction runs inside a turn,
    /// whose own `Stop` ends it, so it reports nothing.
    Compacted,
}

/// Extra launch wiring for a worker so it emits official completion signals.
#[derive(Debug, Clone, Default)]
pub(crate) struct WorkerSignals {
    /// Path to write into `claude --settings <path>` (Claude workers only).
    pub(crate) claude_settings: Option<PathBuf>,
    /// Rudder's Claude plugin and the run's suggestion file (Claude workers only).
    pub(crate) claude_prompt_suggestion: Option<crate::prompt_suggestion::ClaudeWiring>,
    /// Path to point Codex `notify` at (Codex workers only).
    pub(crate) codex_notify: Option<PathBuf>,
    /// Config file to hand opencode through `OPENCODE_CONFIG` (opencode workers
    /// only). It loads Rudder's plugin (opencode_plugin.mjs).
    pub(crate) opencode_config: Option<PathBuf>,
    /// The plugin's environment: the run's signal, session and asks paths.
    pub(crate) opencode_env: Vec<(String, String)>,
    /// The run's hook script, handed to Antigravity's global `rudder` hook
    /// entry through `RUDDER_AGY_HOOK` (Antigravity workers only).
    pub(crate) antigravity_hook: Option<PathBuf>,
}

pub(crate) fn rudder_home() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("RUDDER_HOME") {
        let value = PathBuf::from(value);
        if !value.as_os_str().is_empty() {
            return Some(value);
        }
    }
    let home = std::env::var_os("HOME").filter(|v| !v.is_empty())?;
    Some(PathBuf::from(home).join(".rudder"))
}

pub(crate) fn signals_dir() -> Option<PathBuf> {
    Some(rudder_home()?.join("signals"))
}

pub(crate) fn signal_path(run_id: &str) -> Option<PathBuf> {
    Some(signals_dir()?.join(format!("{run_id}.json")))
}

/// Whether official-signal hooks were wired for this run (its `--settings` /
/// notify config file exists on disk). Interactive workers without this wiring
/// fail visibly instead of guessing completion from terminal output.
pub(crate) fn worker_has_config(run_id: &str, backend: Backend) -> bool {
    let Some(dir) = signals_dir() else {
        return false;
    };
    // Ask "were hooks wired for this RUN", not "for this run under the backend it
    // claims right now". A row's backend can change after launch (switching model
    // provider rewrites it), and keying the check on the current value made the
    // poll loop look for a file that was never going to exist — then KILL a
    // perfectly healthy worker for "lifecycle hooks were not installed".
    // The run id is unique, so any of the three configs proves the wiring ran.
    let _ = backend;
    [
        dir.join(format!("{run_id}-claude.json")),
        dir.join(format!("{run_id}-notify.sh")),
        dir.join(format!("{run_id}-opencode.json")),
        dir.join(format!("{run_id}-agy-hook.sh")),
    ]
    .iter()
    .any(|file| file.exists())
}

/// Read the latest signal a worker wrote, if any.
pub(crate) fn read_signal(run_id: &str) -> Option<SignalState> {
    let body = std::fs::read_to_string(signal_path(run_id)?).ok()?;
    parse_signal_state(&body)
}

/// Remove a consumed signal so it cannot re-fire on a later turn of the same
/// live agent (the worker writes a fresh one when its next turn ends).
pub(crate) fn clear_signal(run_id: &str) {
    if let Some(path) = signal_path(run_id) {
        let _ = std::fs::remove_file(path);
    }
}

/// Remove every per-run signal artifact (the signal file plus the Claude `--settings`
/// / Codex notify config) when a run is deleted: nothing else ever prunes them, so
/// `<rudder_home>/signals/` would otherwise accumulate dead runs forever. Best-effort.
pub(crate) fn cleanup_run_signals(run_id: &str) {
    let Some(dir) = signals_dir() else {
        return;
    };
    let _ = std::fs::remove_file(dir.join(format!("{run_id}.json")));
    let _ = std::fs::remove_file(dir.join(format!("{run_id}-claude.json")));
    let _ = crate::prompt_suggestion::cleanup(&dir, run_id);
    let _ = std::fs::remove_file(dir.join(format!("{run_id}-notify.sh")));
    let _ = std::fs::remove_file(dir.join(format!("{run_id}.json.session")));
    let _ = std::fs::remove_file(dir.join(format!("{run_id}.json.turn")));
    let _ = std::fs::remove_file(dir.join(format!("{run_id}.claude-session")));
    let _ = std::fs::remove_file(dir.join(format!("{run_id}-opencode.json")));
    let _ = std::fs::remove_file(dir.join(format!("{run_id}.opencode-session")));
    let _ = std::fs::remove_file(dir.join(format!("{run_id}-agy-hook.sh")));
    let _ = std::fs::remove_file(dir.join(format!("{run_id}.agy-session")));
    let _ = std::fs::remove_dir_all(asks_dir_in(&dir, run_id));
    let _ = std::fs::remove_dir_all(dir.join(format!("{run_id}.subagents")));
}

/// The phone answers Claude's prompts (approvals, questions, plans) only on the
/// cloud machine, where nobody is at the terminal. Locally the dialog is right
/// there, and a hook that waits on it would only add a process per prompt.
pub(crate) fn remote_asks_enabled() -> bool {
    std::env::var("RUDDER_CLOUD_RUNTIME").ok().as_deref() == Some("home")
}

/// Where a run's pending prompts wait for an answer from the phone
/// (`<id>.json` the request, `<id>.answer.json` the decision).
pub(crate) fn asks_dir_in(signals: &Path, run_id: &str) -> PathBuf {
    signals.join(format!("{run_id}.asks"))
}

pub(crate) const PERMISSION_HOOK_JS: &str = include_str!("permission_hook.mjs");

/// Write the shared hook script (only when it changed) and return it with the
/// run's asks dir.
fn install_permission_hook(signals: &Path, run_id: &str) -> Result<(PathBuf, PathBuf)> {
    let script = signals.join("permission-hook.mjs");
    if std::fs::read_to_string(&script).ok().as_deref() != Some(PERMISSION_HOOK_JS) {
        let tmp = signals.join(format!("permission-hook.mjs.{}.tmp", std::process::id()));
        std::fs::write(&tmp, PERMISSION_HOOK_JS)
            .with_context(|| format!("could not write {}", tmp.display()))?;
        std::fs::rename(&tmp, &script)
            .with_context(|| format!("could not install {}", script.display()))?;
    }
    Ok((script, asks_dir_in(signals, run_id)))
}

/// Add the `PermissionRequest` hook (see permission_hook.mjs) to a Claude
/// settings JSON. It may wait as long as the prompt does, so its timeout is a
/// day rather than Claude's 10 minutes, after which the terminal still works.
pub(crate) fn with_permission_hook(settings_json: &str, script: &Path, asks: &Path) -> String {
    let Ok(mut settings) = serde_json::from_str::<serde_json::Value>(settings_json) else {
        return settings_json.to_string();
    };
    let command = format!(
        "node {} {}",
        shell_quote(&script.display().to_string()),
        shell_quote(&asks.display().to_string())
    );
    if let Some(hooks) = settings.get_mut("hooks").and_then(|value| value.as_object_mut()) {
        hooks.insert(
            "PermissionRequest".to_string(),
            serde_json::json!([{
                "matcher": "*",
                "hooks": [{ "type": "command", "command": command, "timeout": 86400 }]
            }]),
        );
    }
    settings.to_string()
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Remove stale signal/config files that no run deletion ever reached. Best-effort.
pub(crate) fn cleanup_old_signals(max_age: Duration) -> usize {
    let Some(dir) = signals_dir() else {
        return 0;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let Some(cutoff) = SystemTime::now().checked_sub(max_age) else {
        return 0;
    };
    let mut removed = 0_usize;
    for entry in entries.flatten() {
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        if modified >= cutoff {
            continue;
        }
        // A run's subagent payload directory (subagents.rs) that nothing has
        // written to or read from since the cutoff.
        if metadata.is_dir() {
            if entry.file_name().to_string_lossy().ends_with(".subagents")
                && std::fs::remove_dir_all(entry.path()).is_ok()
            {
                removed = removed.saturating_add(1);
            }
            continue;
        }
        if !metadata.is_file() {
            continue;
        }
        if std::fs::remove_file(entry.path()).is_ok() {
            removed = removed.saturating_add(1);
        }
    }
    removed
}

/// Tolerant parse of the signal JSON body into a state.
pub(crate) fn parse_signal_state(body: &str) -> Option<SignalState> {
    let value: serde_json::Value = serde_json::from_str(body.trim()).ok()?;
    match value.get("state").and_then(|s| s.as_str())? {
        "done" => Some(SignalState::Done),
        "input" => Some(SignalState::Input),
        "permission" => Some(SignalState::Permission),
        "working" => Some(SignalState::Working),
        "error" => Some(SignalState::Failed),
        "compacted" => Some(SignalState::Compacted),
        _ => None,
    }
}

/// The optional `detail` a signal carries (the backend's own error type for an
/// `error` signal). Read before `clear_signal` consumes the file.
pub(crate) fn read_signal_detail(run_id: &str) -> Option<String> {
    let body = std::fs::read_to_string(signal_path(run_id)?).ok()?;
    parse_signal_detail(&body)
}

pub(crate) fn parse_signal_detail(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body.trim()).ok()?;
    value
        .get("detail")
        .and_then(|d| d.as_str())
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .map(|d| d.chars().take(80).collect())
}

/// POSIX-sh one-liner that writes `{"state":"<state>"}` to `signal`, creating the
/// parent dir. Single-quoted so the path is taken literally; this runs as a hook
/// command, so it stays tiny (no node/shell startup beyond `sh -c`). Writes to a
/// pid-suffixed temp and `mv`s into place: rename is atomic on the same
/// filesystem, so the TUI poll loop can never read a torn half-written JSON
/// (a bare `printf >` left exactly that window at the moment that matters most).
fn write_signal_command(signal: &Path, state: &str) -> String {
    let path = signal.display();
    format!(
        "mkdir -p '{dir}' && printf '{{\"state\":\"{state}\"}}' > '{path}'.$$.tmp && mv -f '{path}'.$$.tmp '{path}'",
        dir = signal
            .parent()
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    )
}

/// Path of the file a Claude worker's hooks keep its CURRENT session id in.
/// The id Rudder launched with stops being the conversation as soon as the
/// user switches sessions inside Claude (`/resume`, `/clear`): the chat view
/// then read an empty transcript and a reopen resumed the wrong one.
pub(crate) fn claude_session_path(run_id: &str) -> Option<PathBuf> {
    Some(signals_dir()?.join(format!("{run_id}.claude-session")))
}

/// The session id the run's Claude hooks last reported, if any.
pub(crate) fn read_claude_session(run_id: &str) -> Option<String> {
    let text = std::fs::read_to_string(claude_session_path(run_id)?).ok()?;
    let id = text.trim().to_string();
    (!id.is_empty()).then_some(id)
}

/// Hook command: copy `session_id` from the hook's stdin JSON into the run's
/// session file (atomic, like the signal writes).
fn write_claude_session_command(signal: &Path) -> String {
    let path = signal.with_extension("claude-session");
    let path = path.display();
    format!(
        "d=$(sed -n 's/.*\"session_id\": *\"\\([^\"]*\\)\".*/\\1/p' | head -n 1); [ -n \"$d\" ] && printf '%s' \"$d\" > '{path}'.$$.tmp && mv -f '{path}'.$$.tmp '{path}' || true"
    )
}

/// Like `write_signal_command`, but the hook's stdin JSON is scanned for
/// `"<key>":"<value>"` and the value is carried as the signal's `detail`. Plain
/// sed so the hook stays a `sh -c` one-liner with no jq/node dependency; the
/// keys this is used for (Claude's `error`) are closed sets of simple
/// tokens, so the `[^"]*` capture cannot smuggle a quote into the JSON.
fn write_signal_with_detail_command(signal: &Path, state: &str, key: &str) -> String {
    let path = signal.display();
    format!(
        "d=$(sed -n 's/.*\"{key}\":[[:space:]]*\"\\([a-z_][a-z_]*\\)\".*/\\1/p' | head -n 1); mkdir -p '{dir}' && printf '{{\"state\":\"{state}\",\"detail\":\"%s\"}}' \"$d\" > '{path}'.$$.tmp && mv -f '{path}'.$$.tmp '{path}'",
        dir = signal
            .parent()
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    )
}

/// Hook command for the subagent events (subagents.rs): the payload from stdin
/// becomes one file in the run's `<run_id>.subagents/` directory. Written to a
/// temp name and renamed, so the scan never reads half a payload.
fn write_subagent_event_command(signal: &Path) -> String {
    let dir = crate::subagents::events_dir_for_signal(signal);
    format!(
        "d='{dir}'; mkdir -p \"$d\" && f=\"$d/$(date +%s)-$$\" && cat > \"$f.tmp\" && mv -f \"$f.tmp\" \"$f.json\"",
        dir = dir.display()
    )
}

/// The `UserPromptSubmit` side of the subagent events. A `<task-notification>`
/// prompt (Claude reporting a background subagent's end) is kept whole for its
/// status; any other prompt is the human's next turn, and only that fact is
/// written, never the prompt text.
fn write_subagent_prompt_command(signal: &Path) -> String {
    let dir = crate::subagents::events_dir_for_signal(signal);
    format!(
        "p=$(cat); case \"$p\" in *'\"prompt\":\"<task-notification>'*) ;; *) p='{{\"hook_event_name\":\"UserPromptSubmit\"}}' ;; esac; d='{dir}'; mkdir -p \"$d\" && f=\"$d/$(date +%s)-$$\" && printf '%s' \"$p\" > \"$f.tmp\" && mv -f \"$f.tmp\" \"$f.json\"",
        dir = dir.display()
    )
}

/// Hook command for `PostCompact`: writes `compacted` for a manual compaction
/// only. The whole payload is read before matching, so Claude never writes
/// into a pipe the hook already closed. Claude and Codex write compact JSON; the
/// spaced form is accepted too, as in the Codex script.
fn write_compacted_signal_command(signal: &Path) -> String {
    format!(
        "case \"$(cat)\" in *'\"trigger\":\"manual\"'*|*'\"trigger\": \"manual\"'*) {} ;; esac",
        write_signal_command(signal, "compacted")
    )
}

/// The `claude --settings` JSON: a `Stop` hook (turn ended), a `StopFailure` hook
/// (turn ended on an API error), `Notification` hooks for the pause states, and
/// a `PostCompact` hook (a manual `/compact` ended), each writing the signal file.
/// The subagent hooks write payload files for the sidebar tree (subagents.rs).
///
/// Matcher names are Claude's OWN notification types, verified against the
/// installed binary: `permission_prompt`, `idle_prompt`, `agent_needs_input`,
/// `elicitation_dialog`. The permission matcher was first wired as
/// `permission_request` — a name from Claude's remote-bridge protocol, not a
/// notification type — so it never matched and a Claude worker blocked on an
/// approval had no native signal at all.
/// When `fast_mode` is set, also carries `fastMode: true` — the SAME settings key
/// Claude Code's own `/fast` uses — so a launched worker runs in native fast mode
/// (Opus, accelerated output) instead of being faked with a reduced effort level.
pub(crate) fn claude_settings_json(signal: &Path, fast_mode: bool) -> String {
    let mut settings = serde_json::json!({
        "hooks": {
            "Stop": [{
                "hooks": [
                    { "type": "command", "command": write_signal_command(signal, "done") },
                    { "type": "command", "command": write_claude_session_command(signal) }
                ]
            }],
            "Notification": [
                {
                    "matcher": "idle_prompt",
                    "hooks": [{ "type": "command", "command": write_signal_command(signal, "input") }]
                },
                {
                    // AskUserQuestion / teammate prompts ("agent_needs_input") and MCP
                    // elicitation dialogs are questions too, not just the idle prompt.
                    "matcher": "agent_needs_input|elicitation_dialog",
                    "hooks": [{ "type": "command", "command": write_signal_command(signal, "input") }]
                },
                {
                    // Blocked on a tool-approval prompt — the single most common
                    // reason an agent stops dead.
                    "matcher": "permission_prompt",
                    "hooks": [{ "type": "command", "command": write_signal_command(signal, "permission") }]
                }
            ],
            // The turn ended on an API error (rate_limit, overloaded, billing...).
            // `Stop` does NOT fire for these, so without this the row spun forever.
            "StopFailure": [{
                "hooks": [{ "type": "command", "command": write_signal_with_detail_command(signal, "error", "error") }]
            }],
            // The human answered: back to work, clear the latch. Fires once per
            // human turn, so it costs one tiny write — unlike PreToolUse, which
            // would fire on every tool call.
            "UserPromptSubmit": [{
                "hooks": [
                    { "type": "command", "command": write_signal_command(signal, "working") },
                    { "type": "command", "command": write_claude_session_command(signal) },
                    { "type": "command", "command": write_subagent_prompt_command(signal) }
                ]
            }],
            // Subagents, for the tree under the agent's row (subagents.rs).
            // `PostToolUse` is matched to the subagent tool only: its response
            // pairs the new agent's id with its description. Every other tool
            // call runs no hook.
            "SubagentStart": [{
                "hooks": [{ "type": "command", "command": write_subagent_event_command(signal) }]
            }],
            "SubagentStop": [{
                "hooks": [{ "type": "command", "command": write_subagent_event_command(signal) }]
            }],
            "PostToolUse": [{
                "matcher": "Agent|Task",
                "hooks": [{ "type": "command", "command": write_subagent_event_command(signal) }]
            }],
            // A manual `/compact` ended. It fires no `UserPromptSubmit` and no
            // `Stop` (measured on 2.1.318: PreCompact, SessionStart with source
            // "compact", PostCompact), so without this nothing said the agent
            // was back at its prompt. The command ignores automatic compactions:
            // those run inside a turn, and that turn's `Stop` ends it.
            "PostCompact": [{
                "hooks": [{ "type": "command", "command": write_compacted_signal_command(signal) }]
            }]
        }
    });
    if fast_mode {
        if let Some(obj) = settings.as_object_mut() {
            obj.insert("fastMode".to_string(), serde_json::Value::Bool(true));
        }
    }
    settings.to_string()
}

/// The Codex signal script. It is wired TWICE, because Codex has two lifecycle
/// channels and each covers what the other cannot:
///
/// - as the `notify` program: Codex calls it with the event JSON as `$1`; only
///   `agent-turn-complete` exists there, so it reports `done`.
/// - as a lifecycle hook (`hooks.<Event>` config overrides, see
///   `codex_hook_overrides`): Codex pipes the event JSON on stdin with
///   `hook_event_name` set. `Stop`/`Interrupt` -> done, `PermissionRequest` ->
///   permission, `UserPromptSubmit` -> working, `PostCompact` of a manual
///   `/compact` -> compacted (it fires no `Stop` and no `notify`, measured on
///   codex 0.166, and a Codex without the event ignores the override).
///   `SubagentStart` / `SubagentStop` payloads are kept as files for the
///   sidebar's subagent tree (subagents.rs), before the subagent guard below.
///
/// `notify` fires for EVERY thread, not just the conversation (measured on codex
/// 0.162): the hidden title thread Codex runs ~4s into a first turn ("Generate a
/// concise, single-line task title"), and every subagent a turn spawns. Each one
/// used to mark the run done mid-turn and ring "ready for review". So
/// `SessionStart` records the conversation's id (the main thread's id IS the
/// session id) and a `notify` for any other thread is ignored. Hook events that
/// carry an `agent_id` come from a subagent and are ignored too (subagents report
/// `SubagentStop`, never `Stop`, but a guard costs nothing). Before the session
/// id is known (a Codex without hooks), `notify` keeps its old meaning.
///
/// Matching on the literal `"hook_event_name":"Stop"` is deliberate: Codex writes
/// compact JSON (verified against the installed binary), and a looser pattern
/// could match inside `last_assistant_message`.
pub(crate) fn codex_notify_script(signal: &Path) -> String {
    let path = signal.display();
    let dir = signal
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let events = crate::subagents::events_dir_for_signal(signal);
    let events = events.display();
    format!(
        r#"#!/bin/sh
# Rudder Codex lifecycle signal: `notify` program (event in $1) AND hook (event on stdin).
# Temp + mv so the TUI never reads a torn half-written signal.
emit() {{
  mkdir -p '{dir}' && printf '{{"state":"%s"}}' "$1" > '{path}'.$$.tmp && mv -f '{path}'.$$.tmp '{path}'
}}
# One `done` per turn: `Stop` and the conversation's own `notify` both report
# the same turn ~0.6s apart, and a second `done` left unread could end the NEXT
# turn the moment it started.
done_once() {{
  if [ -n "$1" ] && [ "$(cat '{path}.turn' 2>/dev/null)" = "$1" ]; then return 0; fi
  [ -n "$1" ] && printf '%s' "$1" > '{path}.turn'
  emit done
}}
field() {{
  printf '%s' "$2" | sed -n "s/.*\"$1\": *\"\([^\"]*\)\".*/\1/p" | head -n 1
}}
# Subagent events for the tree under the agent's row (subagents.rs).
subagent_event() {{
  mkdir -p '{events}' && f='{events}'/"$(date +%s)-$$" && printf '%s' "$1" > "$f.tmp" && mv -f "$f.tmp" "$f.json"
}}
case "$1" in
  *agent-turn-complete*)
    session=$(cat '{path}.session' 2>/dev/null)
    thread=$(field thread-id "$1")
    if [ -n "$session" ] && [ "$thread" != "$session" ]; then exit 0; fi
    done_once "$(field turn-id "$1")"; exit 0 ;;
  "") ;;
  *) exit 0 ;;
esac
payload=$(cat)
case "$payload" in
  *'"hook_event_name":"SubagentStart"'*|*'"hook_event_name": "SubagentStart"'*|*'"hook_event_name":"SubagentStop"'*|*'"hook_event_name": "SubagentStop"'*)
    subagent_event "$payload"; exit 0 ;;
  *'"agent_id"'*) exit 0 ;;
esac
case "$payload" in
  *'"hook_event_name":"SessionStart"'*|*'"hook_event_name": "SessionStart"'*)
    session=$(field session_id "$payload")
    [ -n "$session" ] && printf '%s' "$session" > '{path}.session' ;;
  *'"hook_event_name":"Stop"'*|*'"hook_event_name": "Stop"'*) done_once "$(field turn_id "$payload")" ;;
  *'"hook_event_name":"Interrupt"'*|*'"hook_event_name": "Interrupt"'*) done_once "$(field turn_id "$payload")" ;;
  *'"hook_event_name":"PermissionRequest"'*|*'"hook_event_name": "PermissionRequest"'*) emit permission ;;
  *'"hook_event_name":"UserPromptSubmit"'*|*'"hook_event_name": "UserPromptSubmit"'*)
    emit working; subagent_event '{{"hook_event_name":"UserPromptSubmit"}}' ;;
  *'"hook_event_name":"PostCompact"'*|*'"hook_event_name": "PostCompact"'*)
    case "$payload" in
      *'"trigger":"manual"'*|*'"trigger": "manual"'*) emit compacted ;;
    esac ;;
esac
exit 0
"#
    )
}

/// The Codex hook events the signal script listens for, mapped onto the protocol.
/// `SessionStart` maps to nothing: it records which thread is the conversation.
/// `SubagentStart` / `SubagentStop` feed the subagent tree (subagents.rs).
pub(crate) const CODEX_HOOK_EVENTS: [&str; 8] = [
    "SessionStart",
    "Stop",
    "Interrupt",
    "PermissionRequest",
    "UserPromptSubmit",
    "PostCompact",
    "SubagentStart",
    "SubagentStop",
];

/// `-c hooks.<Event>=[...]` overrides pointing every lifecycle event Rudder cares
/// about at the signal script, plus the flag that lets them run.
///
/// Codex hooks are trust-gated: a hook definition has to be reviewed in `/hooks`
/// before it runs, which a headless child can never do. `--dangerously-bypass-
/// hook-trust` lifts that for one invocation. Rudder generated this hook itself
/// (it is the vetting), so the bypass is exactly the "automation that already
/// vets hook sources" the flag exists for. It also lets the user's OWN untrusted
/// hooks run inside the worker — the same hooks they would trust in a plain
/// Codex session, so nothing new is executed on their behalf.
///
/// Inline-TOML `-c` values replace the `[hooks]` tables in config.toml for the
/// same event (hooks.json entries still merge in); the script path carries no
/// TOML-special characters (it lives under `RUDDER_HOME`, never the workspace).
pub(crate) fn codex_hook_overrides(script: &Path) -> Vec<String> {
    let mut args = vec!["--dangerously-bypass-hook-trust".to_string()];
    let script = script
        .display()
        .to_string()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    for event in CODEX_HOOK_EVENTS {
        args.push("-c".to_string());
        args.push(format!(
            "hooks.{event}=[{{hooks=[{{type=\"command\",command=\"{script}\"}}]}}]"
        ));
    }
    args
}

/// Rudder's OpenCode plugin (opencode_plugin.mjs).
///
/// OpenCode has no hook flag like Claude's `--settings` or Codex's `notify`; it
/// has plugins, whose `event` hook sees the session, permission and question
/// lifecycle: exactly the states Rudder's signal file carries. One shared file
/// in the signals directory, loaded through the run's `OPENCODE_CONFIG`; the
/// run's own paths reach it in the environment (`opencode_plugin_env`). It is
/// never written into the worker's workspace, so it can never show up in the
/// diff Rudder merges.
pub(crate) const OPENCODE_PLUGIN_JS: &str = include_str!("opencode_plugin.mjs");

/// Where Rudder's OpenCode plugin lives.
pub(crate) fn opencode_plugin_path() -> Option<PathBuf> {
    Some(signals_dir()?.join("opencode-plugin.mjs"))
}

/// Install the plugin for launches that are not wired as workers (a planner's
/// next turn, launch.rs opencode_tui). The dashboard does this at start.
pub(crate) fn install_opencode_plugin() -> Result<PathBuf> {
    let dir = signals_dir().context("no home directory for Rudder's OpenCode plugin")?;
    std::fs::create_dir_all(&dir).with_context(|| format!("could not create {}", dir.display()))?;
    install_opencode_plugin_in(&dir)
}

/// Write the shared OpenCode plugin (only when it changed) and return it.
fn install_opencode_plugin_in(signals: &Path) -> Result<PathBuf> {
    let script = signals.join("opencode-plugin.mjs");
    if std::fs::read_to_string(&script).ok().as_deref() != Some(OPENCODE_PLUGIN_JS) {
        let tmp = signals.join(format!("opencode-plugin.mjs.{}.tmp", std::process::id()));
        std::fs::write(&tmp, OPENCODE_PLUGIN_JS)
            .with_context(|| format!("could not write {}", tmp.display()))?;
        std::fs::rename(&tmp, &script)
            .with_context(|| format!("could not install {}", script.display()))?;
    }
    Ok(script)
}

/// Where the run's OpenCode plugin writes the session the agent is on.
pub(crate) fn opencode_session_path(run_id: &str) -> Option<PathBuf> {
    Some(signals_dir()?.join(format!("{run_id}.opencode-session")))
}

/// The session id the run's OpenCode plugin last reported, if any.
pub(crate) fn read_opencode_session(run_id: &str) -> Option<String> {
    let id = std::fs::read_to_string(opencode_session_path(run_id)?).ok()?.trim().to_string();
    (!id.is_empty()).then_some(id)
}

/// What the plugin needs to know about the run: its signal file, where to
/// report its session and, on the cloud machine, where the phone answers its
/// permissions and questions.
pub(crate) fn opencode_plugin_env(signals: &Path, run_id: &str) -> Vec<(String, String)> {
    let mut env = vec![
        (
            "RUDDER_OPENCODE_SIGNAL".to_string(),
            signals.join(format!("{run_id}.json")).display().to_string(),
        ),
        (
            "RUDDER_OPENCODE_SESSION_FILE".to_string(),
            signals.join(format!("{run_id}.opencode-session")).display().to_string(),
        ),
    ];
    if remote_asks_enabled() {
        env.push((
            "RUDDER_OPENCODE_ASKS".to_string(),
            asks_dir_in(signals, run_id).display().to_string(),
        ));
    }
    env
}

/// The `OPENCODE_CONFIG` payload that loads Rudder's plugin. An absolute path
/// is used deliberately: plugin entries resolve differently depending on where the
/// config lives, and the worker's cwd is a jj workspace we do not want to touch.
pub(crate) fn opencode_config_json(plugin: &Path) -> String {
    serde_json::json!({
        "$schema": "https://opencode.ai/config.json",
        "plugin": [plugin.display().to_string()],
    })
    .to_string()
}

/// Path of the file an Antigravity worker's `SessionStart` hook keeps its
/// conversation id in. Antigravity mints its own ids and has no flag to
/// declare one, so this is how a row learns which conversation to resume.
pub(crate) fn antigravity_session_path(run_id: &str) -> Option<PathBuf> {
    Some(signals_dir()?.join(format!("{run_id}.agy-session")))
}

/// The conversation id the run's Antigravity hooks last reported, if any.
pub(crate) fn read_antigravity_session(run_id: &str) -> Option<String> {
    let text = std::fs::read_to_string(antigravity_session_path(run_id)?).ok()?;
    let id = text.trim().to_string();
    (!id.is_empty()).then_some(id)
}

fn remove_antigravity_session_report(run_id: &str) -> Result<()> {
    let path = antigravity_session_path(run_id).context("no home directory for the run's signals")?;
    match std::fs::remove_file(&path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            Err(error).with_context(|| format!("could not remove {}", path.display()))
        }
        _ => Ok(()),
    }
}

/// The hook events Rudder's global `rudder` entry listens on, in
/// `~/.gemini/config/hooks.json`. Each command passes the event name to the
/// run's script, since Antigravity 1.5.0's payload does not name its event.
pub(crate) const ANTIGRAVITY_HOOK_EVENTS: [&str; 3] = ["SessionStart", "PreInvocation", "Stop"];

/// One hook command: run the script `RUDDER_AGY_HOOK` names, or answer `{}`
/// (no decision, nothing injected) for every Antigravity session Rudder did
/// not launch. Antigravity runs it with `sh -c` and the agent's environment.
pub(crate) fn antigravity_hook_command(event: &str) -> String {
    format!("if [ -n \"$RUDDER_AGY_HOOK\" ]; then exec \"$RUDDER_AGY_HOOK\" {event}; else printf '{{}}'; fi")
}

/// The `rudder` entry of Antigravity's global hooks file. `SessionStart`,
/// `PreInvocation` and `Stop` are flat lists of handlers (docs/hooks.md).
pub(crate) fn antigravity_hooks_entry() -> serde_json::Value {
    let mut entry = serde_json::Map::new();
    for event in ANTIGRAVITY_HOOK_EVENTS {
        entry.insert(
            event.to_string(),
            serde_json::json!([{ "type": "command", "command": antigravity_hook_command(event), "timeout": 10 }]),
        );
    }
    serde_json::Value::Object(entry)
}

/// `hooks.json` text with Rudder's entry set, every other named hook kept as
/// it was. None when the existing text is not a JSON object, which is left
/// for its owner to fix rather than overwritten.
pub(crate) fn antigravity_hooks_with_rudder(existing: Option<&str>) -> Option<String> {
    let mut hooks = match existing.map(str::trim).filter(|text| !text.is_empty()) {
        Some(text) => match serde_json::from_str::<serde_json::Value>(text.trim_start_matches('\u{feff}')) {
            Ok(serde_json::Value::Object(map)) => map,
            _ => return None,
        },
        None => serde_json::Map::new(),
    };
    hooks.insert("rudder".to_string(), antigravity_hooks_entry());
    serde_json::to_string_pretty(&serde_json::Value::Object(hooks)).ok()
}

/// Antigravity's global hooks file: `~/.gemini/config/hooks.json`.
pub(crate) fn antigravity_hooks_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").filter(|home| !home.is_empty())?;
    Some(PathBuf::from(home).join(".gemini").join("config").join("hooks.json"))
}

/// Make sure the global hooks file carries Rudder's entry, rewriting it only
/// when that entry is missing or different. A file that is not a JSON object
/// fails the launch with the reason: the worker could never report a turn.
fn ensure_antigravity_hooks(path: &Path) -> Result<()> {
    let existing = match std::fs::read_to_string(path) {
        Ok(text) => Some(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error).with_context(|| format!("could not read {}", path.display())),
    };
    if let Some(text) = existing.as_deref() {
        let current = serde_json::from_str::<serde_json::Value>(text.trim_start_matches('\u{feff}')).ok();
        if current.as_ref().and_then(|value| value.get("rudder")) == Some(&antigravity_hooks_entry()) {
            return Ok(());
        }
    }
    let body = antigravity_hooks_with_rudder(existing.as_deref())
        .with_context(|| format!("{} is not a JSON object; fix it so Rudder can add its hook", path.display()))?;
    let dir = path.parent().context("the hooks file has no folder")?;
    std::fs::create_dir_all(dir).with_context(|| format!("could not create {}", dir.display()))?;
    let tmp = dir.join(format!("hooks.json.rudder.{}.tmp", std::process::id()));
    std::fs::write(&tmp, body).with_context(|| format!("could not write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("could not install {}", path.display()))
}

/// The per-run Antigravity hook script: the event name in `$1`, the payload
/// (camelCase JSON) on stdin, and a JSON answer on stdout, `{}` for all three
/// events (no decision, nothing injected, the stop allowed).
///
/// Only the agent's own conversation reports. A subagent (`invoke_subagent`)
/// runs inside the agent's process with its environment, so its hooks run
/// this script too; agy lists each subagent in its parent's folder at
/// `brain/<parent>/.system_generated/subagents/<id>.json` before the
/// subagent's `SessionStart`, and every event of a listed conversation is
/// ignored. Otherwise a subagent's id became the run's conversation and its
/// `Stop` ended the run's turn while the agent was still working.
///
/// - `SessionStart` records `conversationId`: the id `--conversation` resumes.
/// - `PreInvocation` with `invocationNum` 0 is the first model call of a
///   turn (agy 1.5.14 counts from 0 on every turn, the first included, and
///   again after a `Stop` that was not fully idle): the human answered, back
///   to work. A one-call turn has no call 1. Later calls of the same turn
///   write nothing.
/// - `Stop` ends the turn when `fullyIdle`: `done`, or `error` with the error
///   text when `terminationReason` is `ERROR`. An interrupt (`USER_CANCELED`)
///   is `done`: the agent is back at its prompt. A `Stop` while subagents or
///   background tasks still run is no turn end: the agent wakes when they
///   report, and its terminal says `done` once that work ends
///   (`antigravity_status_signal`).
pub(crate) fn antigravity_hook_script(signal: &Path, brain: &Path) -> String {
    let path = signal.display();
    let dir = signal
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let session = signal.with_extension("agy-session");
    let session = session.display();
    let brain = brain.display();
    format!(
        r#"#!/bin/sh
# Rudder Antigravity lifecycle signal: hooks.json command, event name in $1, payload on stdin.
# Temp + mv so the TUI never reads a torn half-written signal.
payload=$(cat)
field() {{
  printf '%s' "$payload" | sed -n "s/.*\"$1\": *\"\([^\"]*\)\".*/\1/p" | head -n 1
}}
number() {{
  printf '%s' "$payload" | sed -n "s/.*\"$1\": *\"*\([0-9][0-9]*\).*/\1/p" | head -n 1
}}
emit() {{
  mkdir -p '{dir}' && printf '{{"state":"%s"}}' "$1" > '{path}'.$$.tmp && mv -f '{path}'.$$.tmp '{path}'
}}
id=$(field conversationId)
for listed in '{brain}'/*/.system_generated/subagents/"$id".json; do
  [ -f "$listed" ] && {{ printf '{{}}'; exit 0; }}
done
case "$1" in
  SessionStart)
    [ -n "$id" ] && printf '%s' "$id" > '{session}'.$$.tmp && mv -f '{session}'.$$.tmp '{session}' ;;
  PreInvocation)
    [ "$(number invocationNum)" = "0" ] && emit working ;;
  Stop)
    case "$(field terminationReason)" in
      ERROR)
        detail=$(field error | tr -d '\\"' | cut -c1-80)
        mkdir -p '{dir}' && printf '{{"state":"error","detail":"%s"}}' "$detail" > '{path}'.$$.tmp && mv -f '{path}'.$$.tmp '{path}' ;;
      *) printf '%s' "$payload" | grep -q '"fullyIdle": *true' && emit done ;;
    esac ;;
esac
printf '{{}}'
"#
    )
}

/// The wait an Antigravity OSC 7501 status describes. `blocked` on a
/// permission prompt is `Permission`; blocked on anything else (a question,
/// a sign-in) waits for an answer. `working` lifts a wait. `done` is the end
/// of a turn whose subagents or background tasks outlived its `Stop` hook:
/// agy reports it once that work ends (daemon tasks such as dev servers do
/// not count). An orchestrator reads no turn signals (its plan ends its
/// turn), so its `done` means nothing here. `failed` is left to the `Stop`
/// hook, which carries the error text.
pub(crate) fn antigravity_status_signal(
    status: &rudder_native::pty_terminal::AgentTerminalStatus,
    orchestrator: bool,
) -> Option<SignalState> {
    match (status.state.as_str(), status.kind.as_str()) {
        ("blocked", "permission") => Some(SignalState::Permission),
        ("blocked", _) => Some(SignalState::Input),
        ("working", _) => Some(SignalState::Working),
        ("done", _) if !orchestrator => Some(SignalState::Done),
        _ => None,
    }
}

/// Whether this backend+mode is an INTERACTIVE worker that idles between turns
/// (so it needs a hook to signal completion). Headless planner modes run
/// `claude -p` / `codex exec` and complete via process exit, so they are excluded.
pub(crate) fn worker_wants_signals(backend: Backend, mode: AgentMode) -> bool {
    let _ = backend;
    matches!(
        mode,
        AgentMode::Execute
            | AgentMode::Main
            | AgentMode::ReviewAll
            | AgentMode::OneOff
            // The gam reviewer needs hooks for BOTH reasons a worker ever does.
            // Its turn end is what poll_gam_pairs waits on (it routes on the
            // peer reaching AgentStatus::Done, which only a completion signal
            // sets), and the lifecycle sweep kills any non-orchestrator pane
            // that has no config installed. Leaving this mode out did not
            // degrade the reviewer, it killed it within a tick of spawning --
            // silently, in every pair, since the feature shipped.
            | AgentMode::GamAdversarial
    )
}

/// Write the per-run hook config files (clearing any stale signal from a prior
/// turn so it can't be misread as this turn's completion) and return the launch
/// wiring. A worker without it could never report a finished turn, so a
/// failure here fails the launch with the reason.
pub(crate) fn prepare_worker_signals(run_id: &str, backend: Backend) -> Result<WorkerSignals> {
    let dir = signals_dir().context("no home directory for the worker's lifecycle hooks")?;
    std::fs::create_dir_all(&dir).with_context(|| format!("could not create {}", dir.display()))?;
    let signal = dir.join(format!("{run_id}.json"));
    // Clear a stale signal from a previous turn of this same run.
    match std::fs::remove_file(&signal) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("could not clear {}", signal.display())),
    }
    let write = |path: &Path, body: &str| {
        std::fs::write(path, body).with_context(|| format!("could not write {}", path.display()))
    };
    match backend {
        Backend::Claude => {
            let settings = dir.join(format!("{run_id}-claude.json"));
            // Carry the persisted /fast toggle into the worker's settings (Claude
            // fast mode is a settings flag, not a launch flag, so this is how it
            // reaches a spawned worker).
            let fast_mode = crate::config::fast_mode_enabled();
            let mut json = claude_settings_json(&signal, fast_mode);
            if remote_asks_enabled() {
                let (script, asks) = install_permission_hook(&dir, run_id)?;
                json = with_permission_hook(&json, &script, &asks);
            }
            let suggestion = crate::prompt_suggestion::ClaudeWiring::prepare(&dir, run_id)
                .context("could not install the prompt suggestion plugin")?;
            write(&settings, &json)?;
            Ok(WorkerSignals {
                claude_settings: Some(settings),
                claude_prompt_suggestion: Some(suggestion),
                ..WorkerSignals::default()
            })
        }
        Backend::Codex => {
            let script = dir.join(format!("{run_id}-notify.sh"));
            write(&script, &codex_notify_script(&signal))?;
            set_executable(&script)?;
            Ok(WorkerSignals {
                codex_notify: Some(script),
                ..WorkerSignals::default()
            })
        }
        Backend::Opencode => {
            let plugin = install_opencode_plugin_in(&dir)?;
            let config = dir.join(format!("{run_id}-opencode.json"));
            write(&config, &opencode_config_json(&plugin))?;
            Ok(WorkerSignals {
                opencode_config: Some(config),
                opencode_env: opencode_plugin_env(&dir, run_id),
                ..WorkerSignals::default()
            })
        }
        Backend::Antigravity => {
            let hooks = antigravity_hooks_path().context("no home directory for Antigravity's hooks file")?;
            ensure_antigravity_hooks(&hooks)?;
            let brain = crate::antigravity::antigravity_cli_dir()
                .context("no home directory for Antigravity's conversations")?
                .join("brain");
            let script = dir.join(format!("{run_id}-agy-hook.sh"));
            write(&script, &antigravity_hook_script(&signal, &brain))?;
            set_executable(&script)?;
            Ok(WorkerSignals {
                antigravity_hook: Some(script),
                ..WorkerSignals::default()
            })
        }
    }
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("could not make {} executable", path.display()))
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<()> {
    Ok(())
}

/// The cloud worker sets `RUDDER_REMOTE_CONTROL=1` for the panes it hosts.
pub(crate) fn remote_control_enabled() -> bool {
    std::env::var("RUDDER_REMOTE_CONTROL").ok().as_deref() == Some("1")
}

/// Antigravity's Remote Control needs a Google sign-in: with a Gemini API key
/// (`modelProvider: "gemini"` in its settings) `agy --remote-control` exits
/// at launch, so such a home runs its Antigravity panes without it.
pub(crate) fn antigravity_remote_control_available() -> bool {
    let Some(home) = std::env::var_os("HOME").filter(|home| !home.is_empty()) else {
        return false;
    };
    let settings = PathBuf::from(home).join(".gemini").join("antigravity-cli").join("settings.json");
    let provider = std::fs::read_to_string(settings)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(text.trim_start_matches('\u{feff}')).ok())
        .and_then(|value| value.get("modelProvider").and_then(|v| v.as_str()).map(str::to_string));
    provider.as_deref() != Some("gemini")
}

/// Claude's Remote Control puts a session in the Claude phone app and on
/// claude.ai. The cloud worker turns it on for every Claude pane it hosts
/// (`RUDDER_REMOTE_CONTROL=1`); the name tells the sessions apart in the app.
pub(crate) fn remote_control_session_name(run_id: &str) -> Option<String> {
    if !remote_control_enabled() {
        return None;
    }
    let repo = std::env::var("RUDDER_REPO_NAME")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .and_then(|dir| dir.file_name().map(|name| name.to_string_lossy().to_string()))
        })
        .unwrap_or_else(|| "rudder".to_string());
    let short = run_short_key(run_id);
    // "(cloud)": in the Claude app these sit next to laptop sessions.
    Some(format!("{repo} · {short} (cloud)"))
}

/// Append the official-signal launch flags to an already-built worker command.
/// Claude gets `--settings <file>` (Stop + idle Notification hooks); Codex gets a
/// trailing `-c notify=["<script>"]` that OVERRIDES the earlier `notify=[]` baked
/// by the Codex capability profile (last `-c` wins). No-op for non-worker
/// modes. When the hook files can't be written the launch fails with the
/// reason: the worker could never report a finished turn. The command is put
/// on the run's `account` first (`apply_account`), for every mode; an account
/// that is gone fails the launch.
pub(crate) fn augment_worker_command(
    command: &mut TerminalCommand,
    backend: Backend,
    mode: AgentMode,
    run_id: &str,
    account: Option<&str>,
) -> Result<()> {
    // Here as well as at spawn: the wrappers below (a managed profile, a gam
    // link) start the agent through `/bin/sh` or Rudder itself, which the
    // spawn-time check cannot see through.
    rudder_native::pty_terminal::require_agent_program(command)?;
    apply_account(command, backend, account)?;
    if cfg!(target_os = "linux") && home_runtime_from_env() {
        let volume = std::env::var("RUDDER_HOME_VOLUME").unwrap_or_else(|_| "/workspace".to_string());
        with_home_scratch(command, Path::new(&volume), run_id);
    }
    if !worker_wants_signals(backend, mode) {
        return Ok(());
    }
    // Flattened, so a caller's `error.to_string()` names the cause.
    let signals = prepare_worker_signals(run_id, backend).map_err(|error| {
        anyhow::anyhow!("could not install the worker's lifecycle hooks: {error:#}")
    })?;
    match backend {
        Backend::Claude => {
            if let Some(path) = signals.claude_settings {
                command.args.push("--settings".to_string());
                command.args.push(path.display().to_string());
            }
            if let Some(suggestion) = &signals.claude_prompt_suggestion {
                suggestion.apply(command);
            }
            if !managed_claude_requested(command) {
                if let Some(name) = remote_control_session_name(run_id) {
                command.args.push("--remote-control".to_string());
                    command.args.push(name);
                }
            }
        }
        Backend::Codex => {
            if let Some(path) = signals.codex_notify {
                // Replace the `notify=[]` that the Codex worker profile baked
                // in (its preceding `-c` stays), so this works regardless of how Codex
                // resolves duplicate `-c` keys.
                let replacement = format!("notify=[\"{}\"]", path.display());
                if let Some(slot) = command
                    .args
                    .iter_mut()
                    .find(|arg| arg.as_str() == "notify=[]")
                {
                    *slot = replacement;
                } else {
                    command.args.push("-c".to_string());
                    command.args.push(replacement);
                }
                // Lifecycle hooks go BEFORE any subcommand (`resume <id>`): they
                // are root-level flags, and a `resume` launch carries the session
                // id as a trailing positional that must stay last.
                let at = command
                    .args
                    .iter()
                    .position(|arg| arg == "resume" || arg == "exec")
                    .unwrap_or(command.args.len());
                let hooks = codex_hook_overrides(&path);
                command.args.splice(at..at, hooks);
            }
        }
        Backend::Opencode => {
            if let Some(path) = signals.opencode_config {
                // OpenCode reads an extra config file from this env var; the
                // config loads Rudder's plugin, and the plugin's env names the
                // run's files.
                command
                    .env
                    .push(("OPENCODE_CONFIG".to_string(), path.display().to_string()));
                command.env.extend(signals.opencode_env);
            }
        }
        Backend::Antigravity => {
            if let Some(path) = signals.antigravity_hook {
                command
                    .env
                    .push(("RUDDER_AGY_HOOK".to_string(), path.display().to_string()));
            }
            // The pane answers no OSC 7501 support query, so status
            // reporting is turned on outright (antigravity_status_signal).
            command.env.push(("AGY_CLI_OSC7501".to_string(), "on".to_string()));
            // Antigravity's Remote Control: the session in the Antigravity app
            // and on the web, like Claude's. It takes no name, so the app lists
            // it under the machine.
            if remote_control_enabled() && antigravity_remote_control_available() {
                if command.args.iter().any(|arg| arg == "--conversation") {
                    // A reopened conversation is live before the connection
                    // starts, and the connection follows it.
                    command.args.push("--remote-control".to_string());
                } else {
                    // agy 1.5.14 with `--remote-control` and no conversation
                    // yet makes two at startup, one for the first prompt and
                    // one for the connection, and shows whichever came last.
                    // The prompt then sits in a chat the pane does not show.
                    // So the App types `/remote-control` once the SessionStart
                    // hook names the conversation (start_typed_remote_control);
                    // agy runs it at once, even mid-turn, on that conversation.
                    // A report left from an earlier conversation would set it
                    // off before this one exists.
                    remove_antigravity_session_report(run_id)?;
                    command.env.push((
                        rudder_native::pty_terminal::TYPED_REMOTE_CONTROL_ENV.to_string(),
                        "1".to_string(),
                    ));
                }
            }
        }
    }
    crate::fabric_runtime::prepare_agent_tools(command, backend, run_id)
        .and_then(|()| prepare_managed_profile(command, backend, run_id))
        .map_err(|error| anyhow::anyhow!("the engine could not issue the agent's identity: {error:#}"))
}

/// On the cloud home each agent gets its own scratch directory,
/// `<volume>/scratch/<run id>`, as `RUDDER_SCRATCH`, with `TMPDIR` inside it,
/// so temporary files stop piling up at the top of the volume. The hourly
/// cleanup (cloud/worker/cleanup.mjs) deletes it after the run finishes; it is
/// created at every launch, so a resumed agent gets it back.
pub(crate) fn with_home_scratch(command: &mut TerminalCommand, volume: &Path, run_id: &str) {
    let valid = !run_id.is_empty()
        && !run_id.starts_with('.')
        && run_id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if !valid {
        return;
    }
    let scratch = volume.join("scratch").join(run_id);
    let tmp = scratch.join("tmp");
    if std::fs::create_dir_all(&tmp).is_err() {
        return;
    }
    command.env.retain(|(key, _)| key != "RUDDER_SCRATCH" && key != "TMPDIR");
    command.env.push(("RUDDER_SCRATCH".into(), scratch.display().to_string()));
    command.env.push(("TMPDIR".into(), tmp.display().to_string()));
}

#[cfg(test)]
mod home_scratch_tests {
    use super::*;

    #[test]
    fn each_agent_gets_its_own_scratch_and_temp_dir() {
        let volume = std::env::temp_dir().join(format!("rudder-scratch-{}", std::process::id()));
        let mut command = TerminalCommand::with_args("claude", ["--x"]);
        command.env.push(("TMPDIR".into(), "/tmp".into()));
        with_home_scratch(&mut command, &volume, "run-1a2b");
        let scratch = volume.join("scratch").join("run-1a2b");
        assert!(scratch.join("tmp").is_dir());
        assert!(command.env.contains(&("RUDDER_SCRATCH".into(), scratch.display().to_string())));
        assert!(command.env.contains(&("TMPDIR".into(), scratch.join("tmp").display().to_string())));
        assert_eq!(command.env.iter().filter(|(key, _)| key == "TMPDIR").count(), 1);
        for bad in ["", "..", "../code", "a/b", ".hidden"] {
            let mut other = TerminalCommand::with_args("claude", ["--x"]);
            with_home_scratch(&mut other, &volume, bad);
            assert!(other.env.is_empty(), "{bad:?} gets no scratch");
        }
        let _ = std::fs::remove_dir_all(&volume);
    }
}

/// A saved conversation keeps its original renderer/control contract. A new
/// fabric run can select a managed profile without converting old resumes.
pub(crate) fn augment_existing_worker_command(
    command: &mut TerminalCommand, backend: Backend, mode: AgentMode,
    run_id: &str, profile: Option<&str>, account: Option<&str>,
) -> Result<()> {
    if profile_override(command).is_none() {
        command.env.push(("RUDDER_EXECUTION_PROFILE".into(), profile.unwrap_or("native").into()));
    }
    augment_worker_command(command, backend, mode, run_id, account)
}

/// The env var and config dir that put a `backend` agent on `account`,
/// checked to still exist. `None` for the main account: the agent then reads
/// whatever config dir the dashboard itself has.
pub(crate) fn account_env(backend: Backend, account: Option<&str>) -> Result<Option<(&'static str, PathBuf)>> {
    let Some(id) = account else {
        return Ok(None);
    };
    let (key, name) = match backend {
        Backend::Claude => ("CLAUDE_CONFIG_DIR", "Claude Code"),
        Backend::Codex => ("CODEX_HOME", "Codex"),
        Backend::Opencode => anyhow::bail!("opencode has only its main account; switch this chat to main"),
        Backend::Antigravity => anyhow::bail!("Antigravity has only its main account; switch this chat to main"),
    };
    anyhow::ensure!(valid_account_id(id), "{id:?} is not an account id");
    let dir = account_dir(backend, id).context("the Rudder home is unknown: HOME is not set")?;
    anyhow::ensure!(dir.is_dir(), "the {name} account {id} is gone; switch this chat to another account");
    Ok(Some((key, dir)))
}

/// Put an agent CLI launched for a run on that run's account: an extra
/// account's dir as `CLAUDE_CONFIG_DIR` / `CODEX_HOME`. The main account
/// changes no env. Every spawn of an agent CLI for a run goes through here,
/// directly or through `augment_worker_command`.
pub(crate) fn apply_account(command: &mut TerminalCommand, backend: Backend, account: Option<&str>) -> Result<()> {
    if let Some((key, dir)) = account_env(backend, account)? {
        command.env.retain(|(name, _)| name != key);
        command.env.push((key.to_string(), dir.display().to_string()));
    }
    Ok(())
}

fn profile_override(command: &TerminalCommand) -> Option<&str> {
    command.env.iter().rev().find(|(key, _)| key == "RUDDER_EXECUTION_PROFILE").map(|(_, value)| value.as_str())
}

fn managed_claude_requested(command: &TerminalCommand) -> bool {
    match profile_override(command) {
        Some(profile) => profile == "claude-sdk",
        None => std::env::var("RUDDER_CLAUDE_EXECUTION_PROFILE").ok().as_deref() == Some("sdk")
            || (std::env::var("RUDDER_CLAUDE_EXECUTION_PROFILE").ok().as_deref() != Some("native")
                && super::engine_host::current_process_is_fabric_owner()),
    }
}

fn prepare_managed_profile(command: &mut TerminalCommand, backend: Backend, run_id: &str) -> Result<()> {
    let Some(attempt) = command.env.iter().rev().find(|(key, _)| key == "RUDDER_FABRIC_ATTEMPT_ID").map(|(_, value)| value.clone()) else {
        // Profiles require the persistent actor's exact attempt fence.
        anyhow::ensure!(!matches!(profile_override(command), Some("claude-sdk" | "codex-app-server")), "managed execution requires the fabric engine");
        command.env.retain(|(key, _)| key != "RUDDER_AGENT_INITIAL_PROMPT");
        return Ok(());
    };
    let selected = profile_override(command).map(str::to_string).unwrap_or_else(|| match backend {
        Backend::Claude if managed_claude_requested(command) => "claude-sdk".into(),
        Backend::Codex => "codex-app-server".into(),
        _ => "native".into(),
    });
    let launch = match (backend, selected.as_str()) {
        (_, "native") => None,
        (Backend::Claude, "claude-sdk") => {
            let prompt = command.env.iter().rev().find(|(key, _)| key == "RUDDER_AGENT_INITIAL_PROMPT").map(|(_, value)| value.clone()).filter(|value| !value.is_empty());
            Some(super::agent_link::prepare_claude_sdk_command(run_id, &attempt, command, prompt.as_deref())?)
        }
        (Backend::Codex, "codex-app-server") => super::agent_link::prepare_command(run_id, &attempt, backend, command)?,
        _ => anyhow::bail!("unsupported execution profile for backend"),
    };
    command.env.retain(|(key, _)| key != "RUDDER_AGENT_INITIAL_PROMPT" && key != "RUDDER_EXECUTION_PROFILE");
    command.env.push(("RUDDER_EXECUTION_PROFILE".into(), selected));
    if let Some(launch) = launch {
        command.env.push(("RUDDER_AGENT_LINK_LAUNCH".into(), serde_json::to_string(&launch)?));
    }
    Ok(())
}
