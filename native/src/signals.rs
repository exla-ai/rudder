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
//! - **opencode** (`opencode.ai/docs/plugins`): a generated plugin's `event` hook
//!   sees `session.idle`, `session.error`, `permission.asked` / `permission.replied`.
//!
//! All write `<rudder_home>/signals/<run_id>.json` =
//! `{"state":"done"|"input"|"permission"|"working"|"error","detail":"..."}`.
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
}

/// Extra launch wiring for a worker so it emits official completion signals.
#[derive(Debug, Clone, Default)]
pub(crate) struct WorkerSignals {
    /// Path to write into `claude --settings <path>` (Claude workers only).
    pub(crate) claude_settings: Option<PathBuf>,
    /// Path to point Codex `notify` at (Codex workers only).
    pub(crate) codex_notify: Option<PathBuf>,
    /// Config file to hand opencode through `OPENCODE_CONFIG` (opencode workers
    /// only). It registers the generated plugin that reports turn-end.
    pub(crate) opencode_config: Option<PathBuf>,
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
    let _ = std::fs::remove_file(dir.join(format!("{run_id}-notify.sh")));
    let _ = std::fs::remove_file(dir.join(format!("{run_id}.json.session")));
    let _ = std::fs::remove_file(dir.join(format!("{run_id}.json.turn")));
    let _ = std::fs::remove_file(dir.join(format!("{run_id}.claude-session")));
    let _ = std::fs::remove_file(dir.join(format!("{run_id}-opencode.json")));
    let _ = std::fs::remove_file(dir.join(format!("{run_id}-opencode.js")));
    let _ = std::fs::remove_dir_all(asks_dir_in(&dir, run_id));
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
fn install_permission_hook(signals: &Path, run_id: &str) -> Option<(PathBuf, PathBuf)> {
    let script = signals.join("permission-hook.mjs");
    if std::fs::read_to_string(&script).ok().as_deref() != Some(PERMISSION_HOOK_JS) {
        let tmp = signals.join(format!("permission-hook.mjs.{}.tmp", std::process::id()));
        std::fs::write(&tmp, PERMISSION_HOOK_JS).ok()?;
        std::fs::rename(&tmp, &script).ok()?;
    }
    Some((script, asks_dir_in(signals, run_id)))
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
        if !metadata.is_file() {
            continue;
        }
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        if modified >= cutoff {
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
/// keys this is used for (Claude's `error_type`) are closed sets of simple
/// tokens, so the `[^"]*` capture cannot smuggle a quote into the JSON.
fn write_signal_with_detail_command(signal: &Path, state: &str, key: &str) -> String {
    let path = signal.display();
    format!(
        "d=$(sed -n 's/.*\"{key}\":\"\\([^\"]*\\)\".*/\\1/p' | head -n 1); mkdir -p '{dir}' && printf '{{\"state\":\"{state}\",\"detail\":\"%s\"}}' \"$d\" > '{path}'.$$.tmp && mv -f '{path}'.$$.tmp '{path}'",
        dir = signal
            .parent()
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    )
}

/// The `claude --settings` JSON: a `Stop` hook (turn ended), a `StopFailure` hook
/// (turn ended on an API error), and `Notification` hooks for the pause states,
/// each writing the signal file.
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
                "hooks": [{ "type": "command", "command": write_signal_with_detail_command(signal, "error", "error_type") }]
            }],
            // The human answered: back to work, clear the latch. Fires once per
            // human turn, so it costs one tiny write — unlike PreToolUse, which
            // would fire on every tool call.
            "UserPromptSubmit": [{
                "hooks": [
                    { "type": "command", "command": write_signal_command(signal, "working") },
                    { "type": "command", "command": write_claude_session_command(signal) }
                ]
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
///   permission, `UserPromptSubmit` -> working.
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
  *'"agent_id"'*) exit 0 ;;
esac
case "$payload" in
  *'"hook_event_name":"SessionStart"'*|*'"hook_event_name": "SessionStart"'*)
    session=$(field session_id "$payload")
    [ -n "$session" ] && printf '%s' "$session" > '{path}.session' ;;
  *'"hook_event_name":"Stop"'*|*'"hook_event_name": "Stop"'*) done_once "$(field turn_id "$payload")" ;;
  *'"hook_event_name":"Interrupt"'*|*'"hook_event_name": "Interrupt"'*) done_once "$(field turn_id "$payload")" ;;
  *'"hook_event_name":"PermissionRequest"'*|*'"hook_event_name": "PermissionRequest"'*) emit permission ;;
  *'"hook_event_name":"UserPromptSubmit"'*|*'"hook_event_name": "UserPromptSubmit"'*) emit working ;;
esac
exit 0
"#
    )
}

/// The Codex hook events the signal script listens for, mapped onto the protocol.
/// `SessionStart` maps to nothing: it records which thread is the conversation.
pub(crate) const CODEX_HOOK_EVENTS: [&str; 5] = [
    "SessionStart",
    "Stop",
    "Interrupt",
    "PermissionRequest",
    "UserPromptSubmit",
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

/// The opencode plugin that reports turn-end, generated per run.
///
/// opencode has no hook flag like Claude's `--settings` or Codex's `notify`; it has
/// a plugin API instead. A plugin's `event` hook sees the session and permission
/// lifecycle — exactly the states Rudder's signal file carries. The plugin is
/// loaded from Rudder's own directory via `OPENCODE_CONFIG`, never written into
/// the worker's workspace, so it can never show up in the diff Rudder merges.
///
/// Two things this has to get right that the first version did not:
///
/// 1. **`session.idle` fires for SUBAGENT sessions too.** opencode's `Session`
///    carries a `parentID`, and a child session finishing its work idles just
///    like the root one. Reporting `done` on those marked the whole run complete
///    the first time the agent delegated anything. We resolve the session and
///    ignore any that has a parent.
/// 2. **The ask is `permission.asked`, and it has a counterpart, `permission.replied`.**
///    The first version listened for `permission.updated`, which opencode never
///    emits (verified against the installed binary), so no opencode worker was
///    ever shown as blocked on an approval; and only the ask was wired, so a
///    latch, had it ever set, would never have lifted.
/// 3. **`session.error` is a turn ending too.** A provider error idles the
///    session without `session.idle`, so the row spun as running forever.
///
/// Every lookup fails OPEN: if the session cannot be resolved for any reason
/// (older opencode, changed client shape, server hiccup) we report anyway. A
/// wrong guess about a subagent is a cosmetic early "done"; a silent plugin is a
/// worker that never completes at all, which is far worse.
///
/// Writes to a pid-suffixed temp and renames, like the other two backends, so the
/// poll loop can never read a torn signal.
pub(crate) fn opencode_plugin_js(signal: &Path) -> String {
    let path = signal.display();
    format!(
        r#"// Generated by Rudder. Reports opencode turn state to the dashboard.
import {{ writeFileSync, renameSync, mkdirSync }} from "node:fs";
import {{ dirname }} from "node:path";

const SIGNAL = "{path}";

function report(state, detail) {{
  try {{
    mkdirSync(dirname(SIGNAL), {{ recursive: true }});
    const temp = SIGNAL + "." + process.pid + ".tmp";
    writeFileSync(temp, JSON.stringify(detail ? {{ state, detail }} : {{ state }}));
    renameSync(temp, SIGNAL);
  }} catch {{
    // The dashboard falls back to process exit; never break the agent over this.
  }}
}}

export const RudderSignal = async ({{ client }}) => {{
  // Sessions we have already classified. A run emits many events per session,
  // and resolving each one would put an HTTP round-trip on the event path.
  const isSubagent = new Map();

  async function rootSession(sessionID) {{
    if (!sessionID) return true; // Nothing to check against: fail open.
    if (isSubagent.has(sessionID)) return !isSubagent.get(sessionID);
    let child = false;
    try {{
      const result = await client.session.get({{ path: {{ id: sessionID }} }});
      const session = result?.data ?? result;
      child = Boolean(session?.parentID);
    }} catch {{
      child = false; // Fail open: report rather than go silent.
    }}
    isSubagent.set(sessionID, child);
    return !child;
  }}

  return {{
    event: async ({{ event }}) => {{
      const type = event?.type;
      const sessionID = event?.properties?.sessionID;
      if (type === "session.idle") {{
        // A delegated subagent going idle is not this run finishing its turn.
        if (await rootSession(sessionID)) report("done");
      }} else if (type === "session.error") {{
        if (await rootSession(sessionID)) {{
          const err = event?.properties?.error;
          const detail = String(err?.name ?? err?.data?.message ?? err?.message ?? "error").slice(0, 80);
          report("error", detail);
        }}
      }} else if (type === "permission.asked" || type === "permission.updated") {{
        if (await rootSession(sessionID)) report("permission");
      }} else if (type === "permission.replied") {{
        // The human decided. Back to work — this is what lifts the latch.
        if (await rootSession(sessionID)) report("working");
      }}
    }},
  }};
}};
"#
    )
}

/// The `OPENCODE_CONFIG` payload that loads the generated plugin. An absolute path
/// is used deliberately: plugin entries resolve differently depending on where the
/// config lives, and the worker's cwd is a jj workspace we do not want to touch.
pub(crate) fn opencode_config_json(plugin: &Path) -> String {
    serde_json::json!({
        "$schema": "https://opencode.ai/config.json",
        "plugin": [plugin.display().to_string()],
    })
    .to_string()
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
/// wiring. The poll loop surfaces config-generation failure as a lifecycle error.
pub(crate) fn prepare_worker_signals(run_id: &str, backend: Backend) -> WorkerSignals {
    let Some(dir) = signals_dir() else {
        return WorkerSignals::default();
    };
    if std::fs::create_dir_all(&dir).is_err() {
        return WorkerSignals::default();
    }
    // Clear a stale signal from a previous turn of this same run.
    if let Some(signal) = signal_path(run_id) {
        let _ = std::fs::remove_file(&signal);
        match backend {
            Backend::Claude => {
                let settings = dir.join(format!("{run_id}-claude.json"));
                // Carry the persisted /fast toggle into the worker's settings (Claude
                // fast mode is a settings flag, not a launch flag, so this is how it
                // reaches a spawned worker).
                let fast_mode = crate::config::fast_mode_enabled();
                let mut json = claude_settings_json(&signal, fast_mode);
                if remote_asks_enabled() {
                    if let Some(with_asks) = install_permission_hook(&dir, run_id)
                        .map(|(script, asks)| with_permission_hook(&json, &script, &asks))
                    {
                        json = with_asks;
                    }
                }
                if std::fs::write(&settings, json).is_ok() {
                    return WorkerSignals {
                        claude_settings: Some(settings),
                        ..WorkerSignals::default()
                    };
                }
            }
            Backend::Codex => {
                let script = dir.join(format!("{run_id}-notify.sh"));
                if std::fs::write(&script, codex_notify_script(&signal)).is_ok() {
                    set_executable(&script);
                    return WorkerSignals {
                        codex_notify: Some(script),
                        ..WorkerSignals::default()
                    };
                }
            }
            Backend::Opencode => {
                let plugin = dir.join(format!("{run_id}-opencode.js"));
                let config = dir.join(format!("{run_id}-opencode.json"));
                if std::fs::write(&plugin, opencode_plugin_js(&signal)).is_ok()
                    && std::fs::write(&config, opencode_config_json(&plugin)).is_ok()
                {
                    return WorkerSignals {
                        opencode_config: Some(config),
                        ..WorkerSignals::default()
                    };
                }
            }
        }
    }
    WorkerSignals::default()
}

#[cfg(unix)]
fn set_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        let mut perms = meta.permissions();
        perms.set_mode(0o755);
        let _ = std::fs::set_permissions(path, perms);
    }
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) {}

/// Append the official-signal launch flags to an already-built worker command.
/// Claude gets `--settings <file>` (Stop + idle Notification hooks); Codex gets a
/// trailing `-c notify=["<script>"]` that OVERRIDES the earlier `notify=[]` baked
/// by the Codex capability profile (last `-c` wins). No-op for non-worker
/// modes. Config-generation failure is surfaced by the poll loop.
/// Claude's Remote Control puts a session in the Claude phone app and on
/// claude.ai. The cloud worker turns it on for every Claude pane it hosts
/// (`RUDDER_REMOTE_CONTROL=1`); the name tells the sessions apart in the app.
pub(crate) fn remote_control_session_name(run_id: &str) -> Option<String> {
    if std::env::var("RUDDER_REMOTE_CONTROL").ok().as_deref() != Some("1") {
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

pub(crate) fn augment_worker_command(
    command: &mut TerminalCommand,
    backend: Backend,
    mode: AgentMode,
    run_id: &str,
) {
    if !worker_wants_signals(backend, mode) {
        return;
    }
    let signals = prepare_worker_signals(run_id, backend);
    match backend {
        Backend::Claude => {
            if let Some(path) = signals.claude_settings {
                command.args.push("--settings".to_string());
                command.args.push(path.display().to_string());
            }
            if let Some(name) = remote_control_session_name(run_id) {
                command.args.push("--remote-control".to_string());
                command.args.push(name);
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
                // opencode reads its config from this env var; the config names the
                // generated plugin that reports turn-end.
                command
                    .env
                    .push(("OPENCODE_CONFIG".to_string(), path.display().to_string()));
            }
        }
    }
}
