//! `/gam` transport: how Rudder talks to the two halves of a pair WITHOUT typing
//! into their terminals or reading verdicts back off their screens.
//!
//! The pair loop (`poll_gam_pairs` in main.rs) used to do both: a review packet
//! was bracket-pasted into the reviewer's PTY and its verdict was scraped from
//! the visible lines. Both ends were fragile -- a paste can land in a dialog, a
//! long verdict can wrap or scroll away, and the echoed contract had to be
//! filtered back out of the parse. Each backend now has a real channel:
//!
//! - **Claude Code: channels.** Every Claude half gets a per-run MCP server
//!   (`rudder-native gam-channel`, below) that declares the `claude/channel`
//!   capability. Rudder drops a message into `<dir>/inbox/`, the server pushes
//!   it as a `notifications/claude/channel` event, and Claude Code injects it
//!   into the live session -- mid-turn included. Answers come back as TOOL CALLS
//!   (`gam_verdict` / `gam_reply`) the server writes to `<dir>/outbox/`.
//!   Claude treats channel content as untrusted input by default and refused a
//!   mid-turn steer until the channel was vouched for in the SYSTEM prompt with
//!   a per-run nonce; `claude_link_system_prompt` is that vouching.
//! - **Codex: the app-server.** A Codex half runs its TUI as `codex --remote`
//!   against a private `codex app-server` on a unix socket, so the TUI the user
//!   watches and the JSON-RPC thread Rudder drives are the same session. Rudder
//!   connects as a second client (`spawn_codex_link`) and uses `turn/start`
//!   between turns, `turn/steer` to interject into a running turn, and reads the
//!   final agent message from `turn/completed`. Reviewer turns carry an
//!   `outputSchema`, so the verdict is schema-constrained JSON, not prose.
//!
//! Every path falls back to the old PTY paste + screen parse when its link is
//! not up (channels disabled by policy, an older CLI, a socket that never came
//! up), so a pair degrades instead of hanging.

use super::*;
use anyhow::anyhow;
use std::io::BufRead;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};

/// The MCP server name Claude sees; tool ids are `mcp__rudder_gam__<tool>`.
pub(crate) const GAM_CHANNEL_SERVER: &str = "rudder_gam";
pub(crate) const GAM_VERDICT_TOOL: &str = "gam_verdict";
pub(crate) const GAM_REPLY_TOOL: &str = "gam_reply";
/// How long a channel push may go unacknowledged (no new output from the idle
/// agent) before Rudder assumes the channel is not being consumed and pastes.
pub(crate) const GAM_CHANNEL_ACK_TIMEOUT: Duration = Duration::from_secs(20);
/// After a linked Codex reviewer's turn ends, how long to wait for its
/// `turn/completed` event before falling back to the screen. The notify hook and
/// the websocket race; reading the screen first would miss a JSON verdict.
pub(crate) const GAM_LINK_VERDICT_GRACE: Duration = Duration::from_secs(10);
/// Window after spawn in which the dev-channel consent dialog is answered.
const GAM_CHANNEL_DIALOG_WINDOW: Duration = Duration::from_secs(90);

/// What a message to a linked agent is for. Decides the Codex output schema and
/// whether a running turn may be steered instead of a new one started.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GamSendKind {
    /// A finished turn's diff, for a verdict (reviewer).
    Review,
    /// A mid-turn check (reviewer).
    Interim,
    /// A round's objection, as the generator's next turn.
    Revision,
    /// A course correction for the generator's RUNNING turn.
    Steer,
}

impl GamSendKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Review => "review",
            Self::Interim => "interim",
            Self::Revision => "revision",
            Self::Steer => "steer",
        }
    }

    /// The JSON schema a Codex reviewer's final message must satisfy.
    fn codex_output_schema(self) -> Option<serde_json::Value> {
        let verdicts: &[&str] = match self {
            Self::Review => &["accept", "revise", "escalate", "stop"],
            Self::Interim => &["continue", "steer", "stop"],
            Self::Revision | Self::Steer => return None,
        };
        Some(serde_json::json!({
            "type": "object",
            "properties": {
                "verdict": {"type": "string", "enum": verdicts},
                "message": {"type": "string"}
            },
            "required": ["verdict", "message"],
            "additionalProperties": false
        }))
    }
}

/// Something a link heard from its agent.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum GamSignal {
    /// A structured verdict: a Claude `gam_verdict` call or a Codex reviewer's
    /// schema-constrained final message.
    Verdict(GamVerdict),
    /// The generator's pushback: a Claude `gam_reply` call, or the reply block
    /// lifted from a Codex generator's final message.
    Reply(String),
    /// A Codex turn ended: its final agent message (where a schema verdict
    /// lives) and every agent message of the turn in order (a reply block is
    /// often in an earlier one, ahead of the closing summary).
    TurnCompleted { last: String, all: String },
}

/// Messages from link threads to the App's single-writer loop.
#[derive(Debug)]
pub(crate) enum GamLinkEvent {
    /// The Codex client is connected and subscribed to the agent's thread.
    Ready { run_id: String },
    /// The link heard something.
    Signal { run_id: String, signal: GamSignal },
    /// A send could not be delivered; the App re-delivers it by paste.
    SendFailed { run_id: String, text: String, reason: String },
    /// The link is gone for good; later sends paste.
    Closed { run_id: String, reason: String },
}

pub(crate) enum CodexCommand {
    Send { text: String, kind: GamSendKind },
}

pub(crate) enum GamLinkTransport {
    Claude {
        /// The startup banner said channel messages will be injected.
        channel_live: bool,
        /// The startup banner said they will NOT be (policy, provider, ...).
        channel_refused: bool,
        consent_answered: bool,
        /// A push still waiting for the agent to visibly react.
        pending_ack: Option<(Instant, String, GamSendKind)>,
    },
    Codex {
        tx: Sender<CodexCommand>,
        ready: bool,
        closed: bool,
    },
}

/// One linked agent.
pub(crate) struct GamLink {
    pub(crate) dir: PathBuf,
    pub(crate) nonce: String,
    pub(crate) spawned_at: Instant,
    pub(crate) transport: GamLinkTransport,
}

impl GamLink {
    pub(crate) fn label(&self) -> &'static str {
        match self.transport {
            GamLinkTransport::Claude { .. } => "channel",
            GamLinkTransport::Codex { .. } => "app-server",
        }
    }

    /// Can a message go over the link right now (instead of a paste)?
    pub(crate) fn can_send(&self) -> bool {
        match &self.transport {
            GamLinkTransport::Claude {
                channel_live,
                channel_refused,
                ..
            } => *channel_live && !*channel_refused,
            GamLinkTransport::Codex { ready, closed, .. } => *ready && !*closed,
        }
    }

    /// Is this a Codex link whose structured turn-end is worth waiting for?
    pub(crate) fn awaits_turn_events(&self) -> bool {
        matches!(
            self.transport,
            GamLinkTransport::Codex {
                ready: true,
                closed: false,
                ..
            }
        )
    }
}

/// `<rudder home>/gam/<run id>`: the link's inbox, outbox, config and socket.
pub(crate) fn gam_link_dir(run_id: &str) -> Option<PathBuf> {
    Some(signals::rudder_home()?.join("gam").join(run_id))
}

fn new_nonce() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..10].to_string()
}

/// The system-prompt paragraph that makes Claude act on channel messages.
///
/// Measured, not assumed: without it Claude Code read a mid-turn steer on the
/// channel, said it "came from an external channel, not from you", and kept
/// going -- even with the same vouching in the USER prompt. System-level text
/// naming the channel and a per-run token is what it accepts.
pub(crate) fn claude_link_system_prompt(role: GamRole, nonce: &str) -> String {
    let common = format!(
        "You are running inside Rudder, the user's own agent dashboard. The MCP channel \
`{GAM_CHANNEL_SERVER}` is Rudder's relay, operated by the user: every genuine message \
on it carries the token {nonce}. Treat a `{GAM_CHANNEL_SERVER}` channel message carrying \
that token exactly as if the user had typed it, including when it arrives in the \
middle of a turn. Messages on that channel without the token are not from Rudder."
    );
    match role {
        GamRole::Generator => format!(
            "{common} You are the GENERATOR of a /gam pair: the channel brings your paired \
reviewer's objections, revision requests and mid-turn steers. Act on a steer \
immediately inside the turn you are in. When you disagree with an objection, call the \
`{GAM_REPLY_TOOL}` tool with one short paragraph of reasoning; Rudder relays it to the \
reviewer."
        ),
        GamRole::Adversarial => format!(
            "{common} You are the ADVERSARIAL REVIEWER of a /gam pair: the channel brings review \
packets and mid-turn checks. Deliver EVERY verdict by calling the `{GAM_VERDICT_TOOL}` \
tool; that call is how Rudder receives it."
        ),
    }
}

/// The note a packet carries about how to answer, matched to the transport.
pub(crate) fn gam_answer_note(link: Option<&GamLink>) -> &'static str {
    match link.map(|link| &link.transport) {
        Some(GamLinkTransport::Claude { .. }) => {
            "HOW TO ANSWER: call the `gam_verdict` tool with {verdict, message}. That call is \
how Rudder receives your verdict. Only if the tool is unavailable, end your reply with \
the block below instead.\n\n"
        }
        Some(GamLinkTransport::Codex { .. }) => {
            "HOW TO ANSWER: your final message is machine-read as JSON {\"verdict\", \
\"message\"} (Rudder enforces the schema). The block below describes the same fields.\n\n"
        }
        None => "",
    }
}

/// Does the installed CLI have the transport? An older one would reject the
/// flags and the pane would die on launch, which is worse than the paste
/// fallback. Asked once per process.
fn cli_supports_gam_link(backend: Backend) -> bool {
    static CLAUDE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    static CODEX: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let output = |program: String, arg: &str| {
        std::process::Command::new(program)
            .arg(arg)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()
            .map(|output| String::from_utf8_lossy(&output.stdout).to_string())
            .unwrap_or_default()
    };
    match backend {
        // Channels arrived in Claude Code 2.1.80; the flag is hidden from --help.
        Backend::Claude => *CLAUDE.get_or_init(|| {
            claude_version_supports_channels(&output(claude_program(), "--version"))
        }),
        Backend::Codex => *CODEX.get_or_init(|| output(codex_program(), "--help").contains("--remote")),
        Backend::Opencode => false,
    }
}

pub(crate) fn claude_version_supports_channels(version: &str) -> bool {
    let Some(number) = version.split_whitespace().next() else {
        return false;
    };
    let parts = number
        .split('.')
        .map(|part| part.parse::<u64>().unwrap_or(0))
        .collect::<Vec<_>>();
    match parts.as_slice() {
        [major, minor, patch, ..] => (*major, *minor, *patch) >= (2, 1, 80),
        _ => false,
    }
}

/// Prepare a Claude half: write its MCP config and wire the command.
///
/// The flags go at the FRONT of the argv. `--dangerously-load-development-channels`
/// and `--mcp-config` are variadic, so anything after them that is not an
/// option -- the prompt -- would be swallowed as another server name.
pub(crate) fn wire_claude_link(
    command: &mut TerminalCommand,
    run_id: &str,
    role: GamRole,
) -> Option<GamLink> {
    wire_claude_link_in(command, gam_link_dir(run_id)?, role)
}

pub(crate) fn wire_claude_link_in(
    command: &mut TerminalCommand,
    dir: PathBuf,
    role: GamRole,
) -> Option<GamLink> {
    std::fs::create_dir_all(dir.join("inbox")).ok()?;
    std::fs::create_dir_all(dir.join("outbox")).ok()?;
    let exe = std::env::current_exe().ok()?;
    let role_arg = role.as_str();
    let config = serde_json::json!({
        "mcpServers": {
            GAM_CHANNEL_SERVER: {
                "command": exe.display().to_string(),
                "args": ["gam-channel", dir.display().to_string(), role_arg, "--channel"],
            }
        }
    });
    let config_path = dir.join("mcp.json");
    std::fs::write(&config_path, serde_json::to_vec_pretty(&config).ok()?).ok()?;
    let nonce = new_nonce();
    let tool = match role {
        GamRole::Generator => GAM_REPLY_TOOL,
        GamRole::Adversarial => GAM_VERDICT_TOOL,
    };
    let tool_id = format!("mcp__{GAM_CHANNEL_SERVER}__{tool}");
    // The reviewer runs on an allowlist; its verdict tool has to be on it.
    if let Some(at) = command.args.iter().position(|arg| arg == "--allowedTools") {
        if let Some(list) = command.args.get_mut(at + 1) {
            if !list.split(',').any(|tool| tool == tool_id) {
                list.push(',');
                list.push_str(&tool_id);
            }
        }
    }
    let front = vec![
        "--mcp-config".to_string(),
        config_path.display().to_string(),
        "--dangerously-load-development-channels".to_string(),
        format!("server:{GAM_CHANNEL_SERVER}"),
        "--append-system-prompt".to_string(),
        claude_link_system_prompt(role, &nonce),
    ];
    command.args.splice(0..0, front);
    Some(GamLink {
        dir,
        nonce,
        spawned_at: Instant::now(),
        transport: GamLinkTransport::Claude {
            channel_live: false,
            channel_refused: false,
            consent_answered: false,
            pending_ack: None,
        },
    })
}

/// The shell that runs a Codex half: a private app-server on a unix socket,
/// then the TUI attached to it with `--remote`, so the pane the user watches
/// and the thread Rudder drives are one session. The server is in the pane's
/// process group, so stopping the pane (a group SIGKILL) takes it down too.
const CODEX_LINK_SCRIPT: &str = r#"sock="$1"; log="$2"; shift 2
"$1" app-server --listen "unix://$sock" >"$log" 2>&1 &
srv=$!
trap 'kill $srv 2>/dev/null' EXIT HUP INT TERM
i=0
while [ ! -S "$sock" ] && [ $i -lt 400 ]; do sleep 0.05; i=$((i+1)); done
prog="$1"; shift
"$prog" --remote "unix://$sock" "$@""#;

/// Prepare a Codex half: rewrite its command to run behind a private
/// app-server and start the client thread that will drive it.
pub(crate) fn wire_codex_link(
    command: &mut TerminalCommand,
    run_id: &str,
    events: Sender<GamLinkEvent>,
) -> Option<GamLink> {
    let dir = gam_link_dir(run_id)?;
    std::fs::create_dir_all(&dir).ok()?;
    let socket = dir.join("cx.sock");
    let _ = std::fs::remove_file(&socket);
    let log = dir.join("app-server.log");
    let mut args = vec![
        "-c".to_string(),
        CODEX_LINK_SCRIPT.to_string(),
        "rudder-gam-codex".to_string(),
        socket.display().to_string(),
        log.display().to_string(),
        command.program.clone(),
    ];
    args.append(&mut command.args);
    command.program = "/bin/sh".to_string();
    command.args = args;
    let tx = spawn_codex_link(
        run_id.to_string(),
        socket,
        signals::signal_path(run_id),
        events,
    );
    Some(GamLink {
        dir,
        nonce: new_nonce(),
        spawned_at: Instant::now(),
        transport: GamLinkTransport::Codex {
            tx,
            ready: false,
            closed: false,
        },
    })
}

/// Queue a message for a Claude half's channel server.
pub(crate) fn push_claude_channel(
    dir: &Path,
    nonce: &str,
    text: &str,
    kind: GamSendKind,
) -> std::io::Result<()> {
    let body = serde_json::json!({
        "content": format!("[rudder gam · {} · {}]\n{}", nonce, kind.as_str(), text),
        "meta": {"kind": kind.as_str()},
    });
    let inbox = dir.join("inbox");
    std::fs::create_dir_all(&inbox)?;
    let name = format!("{:020}", now_nanos());
    let tmp = inbox.join(format!(".{name}.tmp"));
    std::fs::write(&tmp, serde_json::to_vec(&body)?)?;
    std::fs::rename(&tmp, inbox.join(format!("{name}.json")))
}

fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default()
}

/// Drain a Claude half's outbox into signals, oldest first.
pub(crate) fn drain_claude_outbox(dir: &Path) -> Vec<GamSignal> {
    let outbox = dir.join("outbox");
    let mut names = match std::fs::read_dir(&outbox) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|name| name.ends_with(".json") && !name.starts_with('.'))
            .collect::<Vec<_>>(),
        Err(_) => return Vec::new(),
    };
    names.sort();
    let mut signals = Vec::new();
    for name in names {
        let path = outbox.join(&name);
        let parsed = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
        let _ = std::fs::remove_file(&path);
        let Some(value) = parsed else { continue };
        let arguments = value.get("arguments").cloned().unwrap_or_default();
        match value.get("tool").and_then(serde_json::Value::as_str) {
            Some(GAM_VERDICT_TOOL) => {
                if let Some(verdict) = verdict_from_value(&arguments) {
                    signals.push(GamSignal::Verdict(verdict));
                }
            }
            Some(GAM_REPLY_TOOL) => {
                let message = arguments
                    .get("message")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                if !message.is_empty() {
                    signals.push(GamSignal::Reply(truncate_chars(
                        &message,
                        GAM_MESSAGE_BUDGET_CHARS,
                    )));
                }
            }
            _ => {}
        }
    }
    signals
}

/// A `{verdict, message}` object as a verdict. Same vocabulary as the block
/// parser; an unknown verdict with a message is still an objection.
pub(crate) fn verdict_from_value(value: &serde_json::Value) -> Option<GamVerdict> {
    let message = value
        .get("message")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    match value.get("verdict").and_then(serde_json::Value::as_str) {
        Some("accept") => Some(GamVerdict::Accept(message)),
        Some("revise") => Some(GamVerdict::Revise(message)),
        Some("escalate") => Some(GamVerdict::Escalate(message)),
        Some("stop") | Some("halt") => Some(GamVerdict::Stop(message)),
        Some("steer") | Some("nudge") => Some(GamVerdict::Steer(message)),
        Some("continue") | Some("none") | Some("ok") => Some(GamVerdict::Continue(message)),
        _ if !message.is_empty() => Some(GamVerdict::Revise(message)),
        _ => None,
    }
}

/// A final agent message as a verdict: schema JSON first, then the block.
pub(crate) fn verdict_from_text(text: &str) -> Option<GamVerdict> {
    let stripped = strip_code_fences(text);
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&stripped) {
        if let Some(verdict) = verdict_from_value(&value) {
            return Some(verdict);
        }
    }
    let lines = text.lines().map(str::to_string).collect::<Vec<_>>();
    parse_gam_verdict(&lines)
}

/// The visible lines as one whitespace-collapsed string. Claude's banner and
/// consent dialog WRAP in a half-width split pane, so a phrase can straddle two
/// lines; matching per line missed them live.
fn joined_screen(lines: &[String]) -> String {
    lines
        .iter()
        .flat_map(|line| line.split_whitespace())
        // A wrapped banner repeats its gutter bar on the continuation line.
        .filter(|token| !token.chars().all(|ch| matches!(ch, '▎' | '│' | '┃' | '▏')))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Read the startup banner of a Claude half: did channels come up?
/// Returns Some(true) live, Some(false) refused, None not shown yet.
pub(crate) fn claude_channel_banner(lines: &[String]) -> Option<bool> {
    let screen = joined_screen(lines);
    if !screen.contains("Channels") && !screen.contains(GAM_CHANNEL_SERVER) {
        return None;
    }
    if screen.contains("blocked by org policy")
        || screen.contains("Channels are not currently available")
        || screen.contains("not available on third-party")
        || screen.contains("waiting for your organization")
        || screen.contains(&format!("server:{GAM_CHANNEL_SERVER} ignored ("))
    {
        return Some(false);
    }
    if screen.contains("inject directly in this session") {
        return Some(true);
    }
    None
}

/// The dev-channel consent dialog Claude Code shows before loading a
/// `server:` channel. Rudder wrote that server itself, so it answers for it.
pub(crate) fn claude_channel_consent_showing(lines: &[String]) -> bool {
    let screen = joined_screen(lines);
    screen.contains("Loading development channels")
        && screen.contains("I am using this for local development")
}

impl App {
    /// Wire a freshly built `/gam` command onto its transport, if enabled.
    pub(crate) fn wire_gam_link(
        &mut self,
        command: &mut TerminalCommand,
        backend: Backend,
        run_id: &str,
        role: GamRole,
    ) {
        if !self.gam_links_enabled || !cli_supports_gam_link(backend) {
            return;
        }
        let link = match backend {
            Backend::Claude => wire_claude_link(command, run_id, role),
            Backend::Codex => wire_codex_link(command, run_id, self.gam_link_tx.clone()),
            Backend::Opencode => None,
        };
        if let Some(link) = link {
            self.gam_links.insert(run_id.to_string(), link);
        }
    }

    /// Send `text` to a `/gam` half over its link, falling back to a paste.
    /// Returns whether it was delivered (or handed to a live link).
    pub(crate) fn gam_send(&mut self, index: usize, text: &str, kind: GamSendKind) -> bool {
        let Some(run_id) = self.agents.get(index).map(|run| run.id.clone()) else {
            return false;
        };
        let sent = match self.gam_links.get_mut(&run_id) {
            Some(link) if link.can_send() => match &mut link.transport {
                GamLinkTransport::Claude { pending_ack, .. } => {
                    match push_claude_channel(&link.dir, &link.nonce, text, kind) {
                        Ok(()) => {
                            *pending_ack = Some((Instant::now(), text.to_string(), kind));
                            true
                        }
                        Err(_) => false,
                    }
                }
                GamLinkTransport::Codex { tx, .. } => tx
                    .send(CodexCommand::Send {
                        text: text.to_string(),
                        kind,
                    })
                    .is_ok(),
            },
            _ => false,
        };
        if sent {
            if let Some(run) = self.agents.get_mut(index) {
                run.last_worker_input_at = Some(Instant::now());
            }
            self.dirty = true;
            return true;
        }
        self.live_inject_at(index, text)
    }

    /// The freshest structured verdict a half sent after `since`, consumed.
    pub(crate) fn take_gam_verdict(&mut self, run_id: &str, since: Option<Instant>) -> Option<GamVerdict> {
        let inbox = self.gam_signals.get_mut(run_id)?;
        let found = inbox.iter().rposition(|(at, signal)| {
            matches!(signal, GamSignal::Verdict(_)) && since.is_none_or(|since| *at >= since)
        });
        let verdict = found.and_then(|position| match inbox.remove(position).1 {
            GamSignal::Verdict(verdict) => Some(verdict),
            _ => None,
        });
        inbox.retain(|(_, signal)| !matches!(signal, GamSignal::Verdict(_)));
        verdict
    }

    pub(crate) fn has_gam_verdict(&self, run_id: &str, since: Option<Instant>) -> bool {
        self.gam_signals.get(run_id).is_some_and(|inbox| {
            inbox.iter().any(|(at, signal)| {
                matches!(signal, GamSignal::Verdict(_)) && since.is_none_or(|since| *at >= since)
            })
        })
    }

    /// The generator's latest structured reply after `since`, consumed.
    pub(crate) fn take_gam_reply(&mut self, run_id: &str, since: Option<Instant>) -> Option<String> {
        let inbox = self.gam_signals.get_mut(run_id)?;
        let reply = inbox.iter().rev().find_map(|(at, signal)| match signal {
            GamSignal::Reply(text) if since.is_none_or(|since| *at >= since) => Some(text.clone()),
            _ => None,
        });
        inbox.retain(|(_, signal)| !matches!(signal, GamSignal::Reply(_)));
        reply
    }

    /// Whether a Codex half's turn-end event has landed after `since`.
    pub(crate) fn gam_turn_event_since(&self, run_id: &str, since: Option<Instant>) -> bool {
        self.gam_signals.get(run_id).is_some_and(|inbox| {
            inbox.iter().any(|(at, signal)| {
                matches!(signal, GamSignal::TurnCompleted { .. }) && since.is_none_or(|since| *at >= since)
            })
        })
    }

    /// Should the loop hold off reading this reviewer's SCREEN, because a
    /// structured answer is still on its way over the link?
    pub(crate) fn gam_link_answer_pending(&self, peer_index: usize, since: Option<Instant>) -> bool {
        let Some(reviewer) = self.agents.get(peer_index) else {
            return false;
        };
        let Some(link) = self.gam_links.get(&reviewer.id) else {
            return false;
        };
        link.awaits_turn_events()
            && !self.gam_turn_event_since(&reviewer.id, since)
            && reviewer
                .completed_at
                .is_some_and(|at| at.elapsed() < GAM_LINK_VERDICT_GRACE)
    }

    fn push_gam_signal(&mut self, run_id: &str, signal: GamSignal) {
        let inbox = self.gam_signals.entry(run_id.to_string()).or_default();
        inbox.push((Instant::now(), signal));
        // Bounded: a chatty agent cannot grow this forever.
        if inbox.len() > 32 {
            let excess = inbox.len() - 32;
            inbox.drain(..excess);
        }
    }

    /// Once per heartbeat, before the pair loop: drain link events and
    /// outboxes, answer the channel consent dialog, read the startup banner,
    /// and re-deliver anything a link could not.
    pub(crate) fn poll_gam_links(&mut self) {
        if self.gam_links.is_empty() {
            return;
        }
        // Links whose agent row is gone are dropped with their files.
        let live_ids = self
            .agents
            .iter()
            .map(|run| run.id.clone())
            .collect::<std::collections::HashSet<_>>();
        let gone = self
            .gam_links
            .keys()
            .filter(|id| !live_ids.contains(*id))
            .cloned()
            .collect::<Vec<_>>();
        for id in gone {
            if let Some(link) = self.gam_links.remove(&id) {
                let _ = std::fs::remove_dir_all(&link.dir);
            }
            self.gam_signals.remove(&id);
        }

        let mut resend: Vec<(String, String)> = Vec::new();
        while let Ok(event) = self.gam_link_rx.try_recv() {
            match event {
                GamLinkEvent::Ready { run_id } => {
                    let label = match self.gam_links.get_mut(&run_id) {
                        Some(GamLink {
                            transport: GamLinkTransport::Codex { ready, .. },
                            ..
                        }) => {
                            *ready = true;
                            "Codex app-server"
                        }
                        _ => continue,
                    };
                    self.record_activity(format!(
                        "gam: {} linked over the {label} (turn/start + turn/steer, structured verdicts)",
                        self.gam_row_label(&run_id)
                    ));
                }
                GamLinkEvent::Signal { run_id, signal } => {
                    // A Codex half's final message carries its answer.
                    if let GamSignal::TurnCompleted { last, all } = &signal {
                        let role = self
                            .agents
                            .iter()
                            .find(|run| run.id == run_id)
                            .and_then(|run| run.gam.as_ref())
                            .map(|gam| gam.role);
                        match role {
                            Some(GamRole::Adversarial) => {
                                if let Some(verdict) =
                                    verdict_from_text(last).or_else(|| verdict_from_text(all))
                                {
                                    self.push_gam_signal(&run_id, GamSignal::Verdict(verdict));
                                }
                            }
                            Some(GamRole::Generator) => {
                                let lines = all.lines().map(str::to_string).collect::<Vec<_>>();
                                if let Some(reply) = lift_gam_reply(&lines) {
                                    self.push_gam_signal(&run_id, GamSignal::Reply(reply));
                                }
                            }
                            None => {}
                        }
                    }
                    self.push_gam_signal(&run_id, signal);
                    self.dirty = true;
                }
                GamLinkEvent::SendFailed {
                    run_id,
                    text,
                    reason,
                } => {
                    self.record_activity(format!(
                        "gam: link send to {} failed ({}); pasting instead",
                        self.gam_row_label(&run_id),
                        truncate_chars(&reason, 80)
                    ));
                    resend.push((run_id, text));
                }
                GamLinkEvent::Closed { run_id, reason } => {
                    if let Some(GamLink {
                        transport: GamLinkTransport::Codex { closed, .. },
                        ..
                    }) = self.gam_links.get_mut(&run_id)
                    {
                        if !*closed {
                            *closed = true;
                            self.record_activity(format!(
                                "gam: {} link closed ({}); falling back to the pane",
                                self.gam_row_label(&run_id),
                                truncate_chars(&reason, 80)
                            ));
                        }
                    }
                }
            }
        }

        // Claude halves: outbox, consent dialog, banner, delivery acks.
        let claude_ids = self
            .gam_links
            .iter()
            .filter(|(_, link)| matches!(link.transport, GamLinkTransport::Claude { .. }))
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for run_id in claude_ids {
            let Some(index) = self.agents.iter().position(|run| run.id == run_id) else {
                continue;
            };
            let dir = self.gam_links[&run_id].dir.clone();
            for signal in drain_claude_outbox(&dir) {
                self.push_gam_signal(&run_id, signal);
                self.dirty = true;
            }
            let lines = self.agents[index]
                .terminal
                .as_ref()
                .map(|terminal| terminal.visible_lines_snapshot())
                .unwrap_or_default();
            let last_output_at = self.agents[index].last_output_at;
            let label = self.gam_row_label(&run_id);
            let mut answer_consent = false;
            let mut announce: Option<String> = None;
            let link = self.gam_links.get_mut(&run_id).expect("listed above");
            let spawned_at = link.spawned_at;
            if let GamLinkTransport::Claude {
                channel_live,
                channel_refused,
                consent_answered,
                pending_ack,
            } = &mut link.transport
            {
                if !*consent_answered
                    && spawned_at.elapsed() < GAM_CHANNEL_DIALOG_WINDOW
                    && claude_channel_consent_showing(&lines)
                {
                    *consent_answered = true;
                    answer_consent = true;
                }
                if !*channel_live && !*channel_refused {
                    match claude_channel_banner(&lines) {
                        Some(true) => {
                            *channel_live = true;
                            announce = Some(format!(
                                "gam: {label} linked over a Claude channel (pushed packets, gam_verdict/gam_reply tool calls)"
                            ));
                        }
                        Some(false) => {
                            *channel_refused = true;
                            announce = Some(format!(
                                "gam: Claude refused the channel for {label}; using the pane instead"
                            ));
                        }
                        None => {}
                    }
                }
                // An idle agent that got a push prints its arrival line at once.
                // Silence means the push is going nowhere: paste it, stop pushing.
                if let Some((sent_at, text, _)) = pending_ack.clone() {
                    if last_output_at > sent_at {
                        *pending_ack = None;
                    } else if sent_at.elapsed() >= GAM_CHANNEL_ACK_TIMEOUT {
                        *pending_ack = None;
                        *channel_refused = true;
                        resend.push((run_id.clone(), text));
                        announce = Some(format!(
                            "gam: the Claude channel to {label} never answered; pasting instead"
                        ));
                    }
                }
            }
            if answer_consent {
                if let Some(terminal) = self.agents[index].terminal.as_mut() {
                    let _ = terminal.write_input(b"\r");
                }
            }
            if let Some(line) = announce {
                self.record_activity(line);
            }
        }

        for (run_id, text) in resend {
            if let Some(index) = self.agents.iter().position(|run| run.id == run_id) {
                self.live_inject_at(index, &text);
            }
        }
    }

    fn gam_row_label(&self, run_id: &str) -> String {
        self.agents
            .iter()
            .find(|run| run.id == run_id)
            .map(|run| match run.gam.as_ref().map(|gam| gam.role) {
                Some(GamRole::Adversarial) => "the reviewer".to_string(),
                Some(GamRole::Generator) => "the generator".to_string(),
                None => run.id.clone(),
            })
            .unwrap_or_else(|| run_id.to_string())
    }
}

// ---------------------------------------------------------------------------
// The MCP channel server (`rudder-native gam-channel <dir> <role> [--channel]`)
// ---------------------------------------------------------------------------

/// Entry point for the subcommand Claude Code spawns from the link's mcp.json.
pub(crate) fn run_gam_channel_server(args: &[String]) -> Result<()> {
    let dir = PathBuf::from(
        args.first()
            .ok_or_else(|| anyhow!("usage: gam-channel <dir> <role> [--channel]"))?,
    );
    let role = args
        .get(1)
        .and_then(|value| GamRole::parse(value))
        .unwrap_or(GamRole::Adversarial);
    let channel = args.iter().any(|arg| arg == "--channel");
    let stdin = std::io::stdin();
    let stdout: Arc<Mutex<Box<dyn Write + Send>>> = Arc::new(Mutex::new(Box::new(std::io::stdout())));
    serve_gam_channel(stdin.lock(), stdout, &dir, role, channel);
    Ok(())
}

fn mcp_write(out: &Arc<Mutex<Box<dyn Write + Send>>>, value: &serde_json::Value) {
    if let Ok(mut out) = out.lock() {
        let _ = writeln!(out, "{value}");
        let _ = out.flush();
    }
}

/// The server loop, generic over its streams so tests drive it in memory.
pub(crate) fn serve_gam_channel(
    input: impl BufRead,
    out: Arc<Mutex<Box<dyn Write + Send>>>,
    dir: &Path,
    role: GamRole,
    channel: bool,
) {
    let _ = std::fs::create_dir_all(dir.join("inbox"));
    let _ = std::fs::create_dir_all(dir.join("outbox"));
    let mut pumping = false;
    for line in input.lines() {
        let Ok(line) = line else { break };
        let Ok(message) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let id = message.get("id").cloned();
        let method = message
            .get("method")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        match method {
            "initialize" => {
                let version = message
                    .pointer("/params/protocolVersion")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!("2025-06-18"));
                let mut capabilities = serde_json::json!({"tools": {}});
                if channel {
                    capabilities["experimental"] = serde_json::json!({"claude/channel": {}});
                }
                mcp_write(
                    &out,
                    &serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "protocolVersion": version,
                            "capabilities": capabilities,
                            "serverInfo": {"name": GAM_CHANNEL_SERVER, "version": "1"},
                            "instructions": gam_channel_instructions(role),
                        }
                    }),
                );
            }
            "notifications/initialized" => {
                if channel && !pumping {
                    pumping = true;
                    spawn_inbox_pump(dir.to_path_buf(), out.clone());
                }
            }
            "ping" => mcp_write(
                &out,
                &serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {}}),
            ),
            "tools/list" => mcp_write(
                &out,
                &serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {"tools": [gam_tool_definition(role)]}
                }),
            ),
            "tools/call" => {
                let name = message
                    .pointer("/params/name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let arguments = message
                    .pointer("/params/arguments")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({}));
                let expected = match role {
                    GamRole::Generator => GAM_REPLY_TOOL,
                    GamRole::Adversarial => GAM_VERDICT_TOOL,
                };
                let result = if name == expected && write_outbox(dir, &name, &arguments).is_ok() {
                    serde_json::json!({"content": [{"type": "text", "text": "Delivered to Rudder."}]})
                } else {
                    serde_json::json!({
                        "content": [{"type": "text", "text": format!("unknown tool {name}")}],
                        "isError": true
                    })
                };
                mcp_write(
                    &out,
                    &serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result}),
                );
            }
            _ => {
                // Requests we do not serve get a proper error (Claude probes
                // `server/discover` first, and an empty success there makes it
                // treat the server as a newer protocol era). Notifications are
                // ignored.
                if id.is_some() {
                    mcp_write(
                        &out,
                        &serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "error": {"code": -32601, "message": "method not found"}
                        }),
                    );
                }
            }
        }
    }
}

fn gam_channel_instructions(role: GamRole) -> String {
    match role {
        GamRole::Generator => format!(
            "Rudder relays your /gam reviewer on this channel as <channel source=\"{GAM_CHANNEL_SERVER}\">. \
Act on those messages; send pushback with the {GAM_REPLY_TOOL} tool."
        ),
        GamRole::Adversarial => format!(
            "Rudder delivers /gam review packets on this channel as <channel source=\"{GAM_CHANNEL_SERVER}\">. \
Answer every packet by calling {GAM_VERDICT_TOOL}; your transcript text never reaches Rudder."
        ),
    }
}

fn gam_tool_definition(role: GamRole) -> serde_json::Value {
    match role {
        GamRole::Adversarial => serde_json::json!({
            "name": GAM_VERDICT_TOOL,
            "description": "Deliver your /gam verdict to Rudder. Review packets: accept | revise | escalate | stop. Mid-turn checks: continue | steer | stop.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "verdict": {
                        "type": "string",
                        "enum": ["accept", "revise", "escalate", "stop", "continue", "steer"]
                    },
                    "message": {"type": "string", "description": "Your objection, steer, or reason. Under 120 words."}
                },
                "required": ["verdict", "message"]
            }
        }),
        GamRole::Generator => serde_json::json!({
            "name": GAM_REPLY_TOOL,
            "description": "Tell your /gam reviewer why you disagree with its objection. Rudder relays it with your next diff.",
            "inputSchema": {
                "type": "object",
                "properties": {"message": {"type": "string", "description": "One short paragraph."}},
                "required": ["message"]
            }
        }),
    }
}

fn write_outbox(dir: &Path, tool: &str, arguments: &serde_json::Value) -> std::io::Result<()> {
    let outbox = dir.join("outbox");
    std::fs::create_dir_all(&outbox)?;
    let name = format!("{:020}", now_nanos());
    let tmp = outbox.join(format!(".{name}.tmp"));
    std::fs::write(
        &tmp,
        serde_json::to_vec(&serde_json::json!({"tool": tool, "arguments": arguments}))?,
    )?;
    std::fs::rename(&tmp, outbox.join(format!("{name}.json")))
}

/// Push every inbox message to Claude as a channel notification, in order.
fn spawn_inbox_pump(dir: PathBuf, out: Arc<Mutex<Box<dyn Write + Send>>>) {
    std::thread::spawn(move || loop {
        let inbox = dir.join("inbox");
        let mut names = std::fs::read_dir(&inbox)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.file_name().to_string_lossy().to_string())
                    .filter(|name| name.ends_with(".json") && !name.starts_with('.'))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        names.sort();
        for name in names {
            let path = inbox.join(&name);
            let body = std::fs::read(&path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
            let _ = std::fs::remove_file(&path);
            if let Some(body) = body {
                mcp_write(
                    &out,
                    &serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": "notifications/claude/channel",
                        "params": body,
                    }),
                );
            }
        }
        if !dir.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    });
}

// ---------------------------------------------------------------------------
// The Codex app-server client
// ---------------------------------------------------------------------------

type CodexSocket = tungstenite::WebSocket<std::os::unix::net::UnixStream>;

/// Connect to a Codex half's private app-server and drive its thread. Runs on
/// its own thread; talks to the App only through `events`.
pub(crate) fn spawn_codex_link(
    run_id: String,
    socket: PathBuf,
    signal_path: Option<PathBuf>,
    events: Sender<GamLinkEvent>,
) -> Sender<CodexCommand> {
    let (tx, rx) = std::sync::mpsc::channel::<CodexCommand>();
    std::thread::spawn(move || {
        let reason = match codex_link_loop(&run_id, &socket, signal_path.as_deref(), &events, &rx) {
            Ok(()) => "the agent exited".to_string(),
            Err(error) => error.to_string(),
        };
        let _ = events.send(GamLinkEvent::Closed { run_id, reason });
    });
    tx
}

/// Connect, retrying while the app-server starts. The socket path Codex
/// creates is a SYMLINK into its own short directory (`sun_path` is ~104
/// bytes), so the link is resolved before connecting.
fn connect_codex_socket(socket: &Path, deadline: Instant) -> Result<CodexSocket> {
    loop {
        let target = std::fs::read_link(socket)
            .map(|target| {
                if target.is_absolute() {
                    target
                } else {
                    socket.parent().unwrap_or(Path::new("/")).join(target)
                }
            })
            .unwrap_or_else(|_| socket.to_path_buf());
        if let Ok(stream) = std::os::unix::net::UnixStream::connect(&target) {
            let (socket, _) = tungstenite::client::client("ws://localhost/", stream)
                .map_err(|error| anyhow!("websocket handshake failed: {error}"))?;
            return Ok(socket);
        }
        if Instant::now() >= deadline {
            return Err(anyhow!("the Codex app-server socket never came up"));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

struct CodexRpc {
    socket: CodexSocket,
    next_id: u64,
}

impl CodexRpc {
    fn send(&mut self, value: serde_json::Value) -> Result<()> {
        self.socket
            .send(tungstenite::Message::Text(value.to_string()))
            .map_err(|error| anyhow!("send failed: {error}"))
    }

    fn request(&mut self, method: &str, params: serde_json::Value) -> Result<u64> {
        self.next_id += 1;
        let id = self.next_id;
        self.send(serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))?;
        Ok(id)
    }

    /// One inbound message, or None on a read timeout.
    fn read(&mut self) -> Result<Option<serde_json::Value>> {
        match self.socket.read() {
            Ok(tungstenite::Message::Text(text)) => Ok(serde_json::from_str(&text).ok()),
            Ok(tungstenite::Message::Binary(bytes)) => Ok(serde_json::from_slice(&bytes).ok()),
            Ok(_) => Ok(None),
            Err(tungstenite::Error::Io(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                Ok(None)
            }
            Err(error) => Err(anyhow!("connection lost: {error}")),
        }
    }

    /// Block until the response to `id` arrives, handing everything else to
    /// `other`.
    fn wait_for(
        &mut self,
        id: u64,
        deadline: Instant,
        other: &mut dyn FnMut(serde_json::Value),
    ) -> Result<serde_json::Value> {
        loop {
            if Instant::now() >= deadline {
                return Err(anyhow!("timed out waiting for the app-server"));
            }
            let Some(message) = self.read()? else { continue };
            if message.get("id").and_then(serde_json::Value::as_u64) == Some(id)
                && message.get("method").is_none()
            {
                if let Some(error) = message.get("error") {
                    return Err(anyhow!("{}", error));
                }
                return Ok(message.get("result").cloned().unwrap_or_default());
            }
            other(message);
        }
    }
}

/// A completed turn's agent messages, in order.
pub(crate) fn turn_agent_messages(turn: &serde_json::Value) -> Vec<String> {
    turn.get("items")
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter(|item| {
                    item.get("type").and_then(serde_json::Value::as_str) == Some("agentMessage")
                })
                .filter_map(|item| item.get("text").and_then(serde_json::Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn codex_link_loop(
    run_id: &str,
    socket: &Path,
    signal_path: Option<&Path>,
    events: &Sender<GamLinkEvent>,
    commands: &Receiver<CodexCommand>,
) -> Result<()> {
    let socket = connect_codex_socket(socket, Instant::now() + Duration::from_secs(60))?;
    socket
        .get_ref()
        .set_read_timeout(Some(Duration::from_millis(50)))
        .map_err(|error| anyhow!("socket setup failed: {error}"))?;
    let mut rpc = CodexRpc { socket, next_id: 0 };
    let mut ignore = |_message: serde_json::Value| {};
    let id = rpc.request(
        "initialize",
        serde_json::json!({"clientInfo": {"name": "rudder", "version": env!("CARGO_PKG_VERSION")}}),
    )?;
    rpc.wait_for(id, Instant::now() + Duration::from_secs(20), &mut ignore)?;
    rpc.send(serde_json::json!({"jsonrpc": "2.0", "method": "initialized"}))?;

    // The TUI starts the session's thread. The server also hosts an EPHEMERAL
    // thread of its own (it titles the session), which must never be mistaken
    // for it: it is listed alongside, and it completes turns of its own.
    let thread_deadline = Instant::now() + Duration::from_secs(120);
    let fail_early_sends = |commands: &Receiver<CodexCommand>| {
        while let Ok(CodexCommand::Send { text, .. }) = commands.try_recv() {
            let _ = events.send(GamLinkEvent::SendFailed {
                run_id: run_id.to_string(),
                text,
                reason: "link not ready".to_string(),
            });
        }
    };
    let thread_id = 'found: loop {
        let id = rpc.request("thread/loaded/list", serde_json::json!({}))?;
        let result = rpc.wait_for(id, Instant::now() + Duration::from_secs(20), &mut ignore)?;
        let ids = result
            .get("data")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        for thread in ids.iter().filter_map(serde_json::Value::as_str) {
            let id = rpc.request("thread/read", serde_json::json!({"threadId": thread}))?;
            let Ok(read) = rpc.wait_for(id, Instant::now() + Duration::from_secs(20), &mut ignore)
            else {
                continue;
            };
            let ephemeral = read
                .pointer("/thread/ephemeral")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            if !ephemeral {
                break 'found thread.to_string();
            }
        }
        if Instant::now() >= thread_deadline {
            return Err(anyhow!("the Codex TUI never started a thread"));
        }
        fail_early_sends(commands);
        std::thread::sleep(Duration::from_millis(300));
    };
    // The link, not Codex's `notify`, is the completion signal for a linked
    // half: notify fires for the server's title thread too, which marked a
    // generator Done while its first turn was still running.
    let signal = |state: &str| {
        if let Some(path) = signal_path {
            let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
            if std::fs::write(&tmp, format!("{{\"state\":\"{state}\"}}")).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }
    };
    let mut active_turn: Option<String> = None;
    let track = |message: &serde_json::Value, active: &mut Option<String>| {
        if message
            .pointer("/params/threadId")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|thread| thread != thread_id)
        {
            return;
        }
        match message.get("method").and_then(serde_json::Value::as_str) {
            Some("turn/started") => {
                *active = message
                    .pointer("/params/turn/id")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string);
                signal("working");
            }
            Some("turn/completed") => {
                *active = None;
                let messages = message
                    .pointer("/params/turn")
                    .map(turn_agent_messages)
                    .unwrap_or_default();
                // The answer is queued BEFORE the done signal lands, so the
                // heartbeat that sees Done always finds the reply/verdict too.
                let _ = events.send(GamLinkEvent::Signal {
                    run_id: run_id.to_string(),
                    signal: GamSignal::TurnCompleted {
                        last: messages.last().cloned().unwrap_or_default(),
                        all: messages.join("\n"),
                    },
                });
                signal("done");
            }
            _ => {}
        }
    };
    // Resuming subscribes this connection to the thread's events. It fails
    // while the first turn's rollout is still being written ("failed to read
    // thread"), so it is retried rather than treated as fatal.
    let resume_deadline = Instant::now() + Duration::from_secs(120);
    let mut early = Vec::new();
    let resumed = loop {
        let id = rpc.request("thread/resume", serde_json::json!({"threadId": thread_id}))?;
        match rpc.wait_for(id, Instant::now() + Duration::from_secs(20), &mut |message| {
            early.push(message)
        }) {
            Ok(result) => break result,
            Err(error) if Instant::now() < resume_deadline => {
                let _ = error;
                fail_early_sends(commands);
                std::thread::sleep(Duration::from_millis(500));
            }
            Err(error) => return Err(error),
        }
    };
    // Where the thread stood when we subscribed: a turn that ended before
    // then sent its turn/completed to nobody.
    let turns = resumed
        .pointer("/thread/turns")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    if let Some(last) = turns.last() {
        if last.get("status").and_then(serde_json::Value::as_str) == Some("inProgress") {
            active_turn = last
                .get("id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string);
            signal("working");
        } else {
            signal("done");
        }
    }
    for message in &early {
        track(message, &mut active_turn);
    }
    let _ = events.send(GamLinkEvent::Ready {
        run_id: run_id.to_string(),
    });

    // request id -> (text, kind, was a steer) for sends awaiting their answer.
    let mut pending: std::collections::HashMap<u64, (String, GamSendKind, bool)> =
        std::collections::HashMap::new();
    loop {
        loop {
            match commands.try_recv() {
                Ok(CodexCommand::Send { text, kind }) => {
                    let steer = matches!(kind, GamSendKind::Steer) && active_turn.is_some();
                    let input = serde_json::json!([{"type": "text", "text": text}]);
                    let id = if steer {
                        rpc.request(
                            "turn/steer",
                            serde_json::json!({
                                "threadId": thread_id,
                                "expectedTurnId": active_turn,
                                "input": input,
                            }),
                        )?
                    } else {
                        let mut params = serde_json::json!({"threadId": thread_id, "input": input});
                        if let Some(schema) = kind.codex_output_schema() {
                            params["outputSchema"] = schema;
                        }
                        rpc.request("turn/start", params)?
                    };
                    pending.insert(id, (text, kind, steer));
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return Ok(()),
            }
        }
        let Some(message) = rpc.read()? else { continue };
        if message.get("method").is_none() {
            let Some(id) = message.get("id").and_then(serde_json::Value::as_u64) else {
                continue;
            };
            let Some((text, kind, steered)) = pending.remove(&id) else {
                continue;
            };
            if let Some(error) = message.get("error") {
                if steered {
                    // The turn ended under us: start a new one instead.
                    let mut params = serde_json::json!({
                        "threadId": thread_id,
                        "input": [{"type": "text", "text": text}],
                    });
                    if let Some(schema) = kind.codex_output_schema() {
                        params["outputSchema"] = schema;
                    }
                    let id = rpc.request("turn/start", params)?;
                    pending.insert(id, (text, kind, false));
                } else {
                    let _ = events.send(GamLinkEvent::SendFailed {
                        run_id: run_id.to_string(),
                        text,
                        reason: error.to_string(),
                    });
                }
            }
            continue;
        }
        track(&message, &mut active_turn);
    }
}

/// Put the transport's answering note just above a packet's output contract.
pub(crate) fn with_gam_answer_note(packet: String, link: Option<&GamLink>) -> String {
    let note = gam_answer_note(link);
    if note.is_empty() {
        return packet;
    }
    match packet.find("OUTPUT CONTRACT") {
        Some(at) => format!("{}{note}{}", &packet[..at], &packet[at..]),
        None => format!("{packet}\n\n{note}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rgl-{name}-{}", new_nonce()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[derive(Clone)]
    struct Captured(Arc<Mutex<Vec<u8>>>);
    impl Write for Captured {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn responses(buffer: &Arc<Mutex<Vec<u8>>>) -> Vec<serde_json::Value> {
        String::from_utf8_lossy(&buffer.lock().unwrap())
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    #[test]
    fn the_channel_server_speaks_mcp_pushes_the_inbox_and_records_tool_calls() {
        let dir = scratch("server");
        // A packet already waiting: it must be pushed once the client is up.
        push_claude_channel(&dir, "n0nce", "Review round 1", GamSendKind::Review).unwrap();
        let input = [
            r#"{"jsonrpc":"2.0","id":"probe","method":"server/discover","params":{}}"#,
            r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-11-25"}}"#,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"gam_verdict","arguments":{"verdict":"revise","message":"tests are missing"}}}"#,
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"gam_reply","arguments":{"message":"wrong role"}}}"#,
        ]
        .join("\n");
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let out: Arc<Mutex<Box<dyn Write + Send>>> =
            Arc::new(Mutex::new(Box::new(Captured(buffer.clone()))));
        serve_gam_channel(input.as_bytes(), out, &dir, GamRole::Adversarial, true);

        let mut pushed = None;
        for _ in 0..40 {
            pushed = responses(&buffer).into_iter().find(|message| {
                message["method"] == "notifications/claude/channel"
            });
            if pushed.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let all = responses(&buffer);
        let by_id = |id: serde_json::Value| {
            all.iter()
                .find(|message| message["id"] == id)
                .cloned()
                .unwrap_or_default()
        };
        // An unserved request is an ERROR, not an empty success: Claude probes
        // server/discover and treats a success as a newer protocol era.
        assert_eq!(by_id(serde_json::json!("probe"))["error"]["code"], -32601);
        let init = by_id(serde_json::json!(0));
        assert_eq!(init["result"]["protocolVersion"], "2025-11-25");
        assert!(init["result"]["capabilities"]["experimental"]["claude/channel"].is_object());
        assert_eq!(
            by_id(serde_json::json!(1))["result"]["tools"][0]["name"],
            GAM_VERDICT_TOOL
        );
        assert!(by_id(serde_json::json!(3))["result"]["isError"] == true, "the reviewer has no reply tool");
        let pushed = pushed.expect("inbox message pushed as a channel notification");
        let content = pushed["params"]["content"].as_str().unwrap();
        assert!(content.starts_with("[rudder gam · n0nce · review]"), "{content}");
        assert!(content.contains("Review round 1"));
        assert_eq!(
            drain_claude_outbox(&dir),
            vec![GamSignal::Verdict(GamVerdict::Revise("tests are missing".to_string()))]
        );
        assert!(drain_claude_outbox(&dir).is_empty(), "consumed once");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn channels_need_claude_code_2_1_80() {
        assert!(claude_version_supports_channels("2.1.296 (Claude Code)"));
        assert!(claude_version_supports_channels("2.1.80 (Claude Code)"));
        assert!(!claude_version_supports_channels("2.1.79 (Claude Code)"));
        assert!(!claude_version_supports_channels("2.0.99"));
        assert!(!claude_version_supports_channels(""));
    }

    #[test]
    fn verdicts_read_from_schema_json_or_the_block() {
        assert_eq!(
            verdict_from_text(r#"{"verdict":"accept","message":"ship it"}"#),
            Some(GamVerdict::Accept("ship it".to_string()))
        );
        assert_eq!(
            verdict_from_text("```json\n{\"verdict\":\"steer\",\"message\":\"use src/net\"}\n```"),
            Some(GamVerdict::Steer("use src/net".to_string()))
        );
        assert_eq!(
            verdict_from_text(
                "looked at it\nRUDDER_GAM_VERDICT_START\n{\"verdict\":\"stop\",\"message\":\"wrong repo\"}\nRUDDER_GAM_VERDICT_END"
            ),
            Some(GamVerdict::Stop("wrong repo".to_string()))
        );
        assert_eq!(verdict_from_text("no answer here"), None);
    }

    #[test]
    fn the_startup_screen_says_whether_the_channel_is_live() {
        let lines = |text: &str| text.lines().map(str::to_string).collect::<Vec<_>>();
        assert!(claude_channel_consent_showing(&lines(
            "  WARNING: Loading development channels\n  ❯ 1. I am using this for local development\n    2. Exit"
        )));
        assert!(!claude_channel_consent_showing(&lines("❯ 1. Yes")));
        assert_eq!(
            claude_channel_banner(&lines(
                "▎ Channels (experimental) messages from server:rudder_gam inject directly in this session · restart without --dangerously-load-development-channels to stop\n▎ server:rudder_gam · no MCP server configured with that name"
            )),
            Some(true)
        );
        assert_eq!(
            claude_channel_banner(&lines(
                "--dangerously-load-development-channels blocked by org policy (server:rudder_gam)"
            )),
            Some(false)
        );
        assert_eq!(claude_channel_banner(&lines("❯ Try \"fix lint errors\"")), None);
        // Wrapped in a half-width pane: the phrase straddles two lines.
        assert_eq!(
            claude_channel_banner(&lines(
                "▎ Channels (experimental) messages from server:rudder_gam inject\n▎ directly in this session · restart without"
            )),
            Some(true)
        );
    }

    #[test]
    fn a_claude_half_gets_its_channel_flags_ahead_of_the_prompt() {
        let dir = scratch("wire");
        let mut command = TerminalCommand::with_args(
            "claude",
            [
                "--permission-mode",
                "bypassPermissions",
                "--allowedTools",
                "Read,Grep,Glob,LS",
                "--model",
                "opus",
                "review this",
            ]
            .map(str::to_string),
        );
        let link = wire_claude_link_in(&mut command, dir.clone(), GamRole::Adversarial).unwrap();
        let args = &command.args;
        // Variadic flags first: nothing after them may be the prompt.
        assert_eq!(args[0], "--mcp-config");
        assert_eq!(args[2], "--dangerously-load-development-channels");
        assert_eq!(args[3], "server:rudder_gam");
        assert_eq!(args[4], "--append-system-prompt");
        assert!(args[5].contains(&link.nonce), "the system prompt vouches with the nonce");
        assert!(args[5].contains("gam_verdict"));
        assert_eq!(args[6], "--permission-mode", "the original argv follows untouched");
        assert_eq!(args.last().unwrap(), "review this");
        let allowed = &args[args.iter().position(|arg| arg == "--allowedTools").unwrap() + 1];
        assert_eq!(allowed, "Read,Grep,Glob,LS,mcp__rudder_gam__gam_verdict");
        let config: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("mcp.json")).unwrap()).unwrap();
        let server = &config["mcpServers"]["rudder_gam"];
        assert_eq!(server["args"][0], "gam-channel");
        assert_eq!(server["args"][2], "adversarial");
        assert_eq!(server["args"][3], "--channel");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A minimal Codex app-server: answers the handshake, hosts one thread, and
    /// records what the client asked for.
    fn fake_codex_server(
        socket: PathBuf,
        seen: Arc<Mutex<Vec<serde_json::Value>>>,
    ) -> std::thread::JoinHandle<()> {
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut ws = tungstenite::accept(stream).unwrap();
            let mut loaded_calls = 0;
            let send = |ws: &mut tungstenite::WebSocket<std::os::unix::net::UnixStream>,
                        value: serde_json::Value| {
                ws.send(tungstenite::Message::Text(value.to_string())).unwrap();
            };
            loop {
                let Ok(message) = ws.read() else { return };
                let tungstenite::Message::Text(text) = message else { continue };
                let request: serde_json::Value = serde_json::from_str(&text).unwrap();
                seen.lock().unwrap().push(request.clone());
                let id = request["id"].clone();
                match request["method"].as_str().unwrap_or_default() {
                    "initialize" => send(&mut ws, serde_json::json!({"id": id, "result": {}})),
                    "thread/loaded/list" => {
                        loaded_calls += 1;
                        // The TUI has not started its thread on the first ask.
                        let data = if loaded_calls == 1 {
                            serde_json::json!([])
                        } else {
                            serde_json::json!(["thread-1"])
                        };
                        // (The ephemeral title thread is covered by thread/read.)
                        send(&mut ws, serde_json::json!({"id": id, "result": {"data": data}}));
                    }
                    "thread/read" => send(
                        &mut ws,
                        serde_json::json!({"id": id, "result": {"thread": {"ephemeral": false}}}),
                    ),
                    "thread/resume" => send(
                        &mut ws,
                        serde_json::json!({"id": id, "result": {"thread": {"turns": [{"id": "boot", "status": "completed"}]}}}),
                    ),
                    "turn/start" => {
                        send(&mut ws, serde_json::json!({"id": id, "result": {"turn": {"id": "turn-1"}}}));
                        send(&mut ws, serde_json::json!({"method": "turn/started", "params": {"turn": {"id": "turn-1"}}}));
                    }
                    "turn/steer" => {
                        send(&mut ws, serde_json::json!({"id": id, "result": {"turnId": "turn-1"}}));
                        send(&mut ws, serde_json::json!({"method": "turn/completed", "params": {"turn": {"id": "turn-1", "items": [
                            {"type": "userMessage"},
                            {"type": "agentMessage", "text": "RUDDER_GAM_REPLY_START\nearlier note\nRUDDER_GAM_REPLY_END"},
                            {"type": "agentMessage", "text": "{\"verdict\":\"accept\",\"message\":\"good\"}"}
                        ]}}}));
                    }
                    _ => {}
                }
            }
        })
    }

    #[test]
    fn the_codex_link_starts_schema_turns_steers_live_ones_and_reports_the_answer() {
        let dir = scratch("codex");
        let socket = dir.join("cx.sock");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let server = fake_codex_server(socket.clone(), seen.clone());
        let (events_tx, events_rx) = std::sync::mpsc::channel();
        let signal = dir.join("signal.json");
        let commands =
            spawn_codex_link("gam-adv".to_string(), socket, Some(signal.clone()), events_tx);
        let next = |what: &str| {
            events_rx
                .recv_timeout(Duration::from_secs(10))
                .unwrap_or_else(|_| panic!("no event while waiting for {what}"))
        };
        assert!(matches!(next("ready"), GamLinkEvent::Ready { run_id } if run_id == "gam-adv"));
        // Its bootstrap turn finished before we subscribed: that is reported.
        assert_eq!(std::fs::read_to_string(&signal).unwrap(), r#"{"state":"done"}"#);

        commands
            .send(CodexCommand::Send {
                text: "Review round 1".to_string(),
                kind: GamSendKind::Review,
            })
            .unwrap();
        // turn/started has to land before a steer can target the live turn.
        for _ in 0..100 {
            if seen.lock().unwrap().iter().any(|m| m["method"] == "turn/start") {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(Duration::from_millis(200));
        commands
            .send(CodexCommand::Send {
                text: "change course".to_string(),
                kind: GamSendKind::Steer,
            })
            .unwrap();
        match next("turn end") {
            GamLinkEvent::Signal {
                signal: GamSignal::TurnCompleted { last, all },
                ..
            } => {
                assert_eq!(verdict_from_text(&last), Some(GamVerdict::Accept("good".to_string())));
                assert!(all.starts_with("RUDDER_GAM_REPLY_START"), "{all}");
            }
            other => panic!("expected the turn's final message, got {other:?}"),
        }
        // The link is the completion signal for a linked half (written just
        // after the answer is queued).
        let mut state = String::new();
        for _ in 0..50 {
            state = std::fs::read_to_string(&signal).unwrap_or_default();
            if state.contains("done") {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(state, r#"{"state":"done"}"#);
        let seen = seen.lock().unwrap().clone();
        let start = seen.iter().find(|m| m["method"] == "turn/start").unwrap();
        assert_eq!(start["params"]["threadId"], "thread-1");
        assert_eq!(
            start["params"]["outputSchema"]["properties"]["verdict"]["enum"],
            serde_json::json!(["accept", "revise", "escalate", "stop"])
        );
        let steer = seen.iter().find(|m| m["method"] == "turn/steer").unwrap();
        assert_eq!(steer["params"]["expectedTurnId"], "turn-1");
        assert_eq!(steer["params"]["input"][0]["text"], "change course");
        drop(commands);
        drop(server);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
