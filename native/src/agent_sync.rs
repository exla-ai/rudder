//! `/sync`: tell every agent in the project what the others are doing.
//!
//! The summary is built deterministically from run records and what the
//! dashboard already holds in memory (status, task, the worker's completion
//! summary, the newest line of its pane, the sidebar's cached diff stat). No
//! jj, no model call and no file read happens here, so the App loop never
//! waits on it.
//!
//! Delivery types the message into each agent's pane only once that agent is
//! idle at its prompt: its turn ended, nothing waits on the user, no menu is
//! open and nobody has typed into its composer since. A busy agent gets it when
//! its turn ends. Nothing is interrupted and no Esc is sent.
//!
//! A sync is context, not the user talking. It adds no user turn (planned
//! nodes keep merging on their own), never touches `merge_requested`,
//! `user_typed_runs` or `last_user_input_at`, does not count as recent input
//! for the conflict resolver's idle gate, and the reply turn it starts ends
//! without a "ready for review" ping.
use super::*;

/// The whole message stays around this size, so every idle agent's reply
/// turn stays short.
pub(crate) const SYNC_MESSAGE_CAP: usize = 2000;
/// A busy agent whose turn has not ended by then gets nothing: by that point a
/// newer `/sync` is the better source.
pub(crate) const SYNC_PENDING_TTL: Duration = Duration::from_secs(30 * 60);
/// A turn ending this long after a sync was typed in is not its reply.
pub(crate) const SYNC_REPLY_WINDOW: Duration = Duration::from_secs(10 * 60);
/// Input recorded this soon after a sync is the sync itself: its paste and
/// the deferred Enter (`ENTER_SUBMIT_DELAY`) that submits it.
const SYNC_INPUT_SLACK: Duration = Duration::from_secs(3);

pub(crate) const SYNC_PREAMBLE: &str = "Rudder sync (context only): here is what the other agents in this project are doing right now. No reply needed; keep going with your own task, and don't edit files another agent is working on.";

/// One recipient still waiting for its turn to end.
pub(crate) struct PendingSync {
    pub(crate) run_id: String,
    /// The pane the sync was queued for. A restarted or re-goaled agent is a
    /// new conversation and gets nothing stale.
    pub(crate) instance_id: String,
    pub(crate) queued_at: Instant,
}

/// When a sync was typed into a run, and whether the reply turn it started
/// is still open.
pub(crate) struct SyncDelivery {
    pub(crate) at: Instant,
    pub(crate) reply_open: bool,
}

/// One agent's block in the message.
#[derive(Clone, Debug, Default)]
pub(crate) struct SyncEntry {
    pub(crate) name: String,
    pub(crate) backend: String,
    pub(crate) status: String,
    pub(crate) task: Option<String>,
    pub(crate) latest: Option<String>,
    pub(crate) changes: Option<String>,
    pub(crate) blocked: Option<String>,
}

/// A row `/sync` reports on: running, waiting, paused, or finished and not
/// merged. Stopped, failed, merged and deleted rows are gone from the
/// picture; planners, shell panes and rows living in the cloud are not agents
/// working in this project's files.
pub(crate) fn sync_participant(run: &AgentRun) -> bool {
    matches!(
        run.status,
        AgentStatus::Running | AgentStatus::Done | AgentStatus::Paused
    ) && !run.plain_process
        && !matches!(run.mode, AgentMode::Plan | AgentMode::RudderPlan)
        && !is_cloud_owned_agent(run)
}

/// Whether the only input since `at` is the sync itself.
fn only_sync_input_since(run: &AgentRun, at: Instant) -> bool {
    run.last_worker_input_at
        .is_none_or(|input| input <= at + SYNC_INPUT_SLACK)
}

/// Called where a turn ends: true when that turn only answered a sync, so it
/// ends without the review ping. Closes the reply either way.
pub(crate) fn sync_reply_ended(
    deliveries: &mut HashMap<String, SyncDelivery>,
    run: &AgentRun,
) -> bool {
    let Some(delivery) = deliveries.get_mut(&run.id) else {
        return false;
    };
    let open = std::mem::replace(&mut delivery.reply_open, false);
    open && delivery.at.elapsed() < SYNC_REPLY_WINDOW && only_sync_input_since(run, delivery.at)
}

/// Text on one line, without control characters or secrets, cut to `max`.
fn one_line(text: &str, max: usize) -> String {
    let cleaned: String = redact_secret_values(text)
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect();
    let joined = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_chars(&joined, max)
}

/// "3 files changed, 10 insertions(+), 2 deletions(-)" as "3 files, +10 -2".
fn short_diff_stat(summary: &str) -> String {
    let mut parts = Vec::new();
    for part in summary.split(',').map(str::trim) {
        let Some(count) = part.split_whitespace().next() else {
            continue;
        };
        if part.contains("file") {
            parts.push(format!(
                "{count} file{}",
                if count == "1" { "" } else { "s" }
            ));
        } else if part.contains("insertion") {
            parts.push(format!("+{count}"));
        } else if part.contains("deletion") {
            parts.push(format!("-{count}"));
        }
    }
    if parts.is_empty() {
        return one_line(summary, 40);
    }
    let mut out = parts.remove(0);
    if !parts.is_empty() {
        out.push_str(", ");
        out.push_str(&parts.join(" "));
    }
    out
}

/// The newest message or tool line on an agent's screen: Claude marks each
/// with `⏺`, Codex with `•`. Spinner rows are skipped. opencode draws neither.
pub(crate) fn pane_latest_line(backend: Backend, lines: &[String]) -> Option<String> {
    let glyph = match backend {
        Backend::Claude => '\u{23fa}',
        Backend::Codex => '\u{2022}',
        Backend::Opencode => return None,
    };
    lines.iter().rev().find_map(|line| {
        let rest = line.trim().strip_prefix(glyph)?.trim();
        (!rest.is_empty() && !looks_busy(line)).then(|| rest.to_string())
    })
}

fn sync_status(run: &AgentRun) -> &'static str {
    let integrating = run.integration.phase == IntegrationPhase::Integrating;
    match run.status {
        AgentStatus::Running if run.merge_resolver || run.has_merge_conflict() => {
            "resolving a merge conflict"
        }
        AgentStatus::Running if integrating => "merging",
        AgentStatus::Running if run.wait_signal == Some(WaitSignal::Idle) => "idle at its prompt",
        AgentStatus::Running
            if run.needs_permission || run.needs_user_input || run.wait_signal.is_some() =>
        {
            "waiting for the user"
        }
        AgentStatus::Running => "working",
        AgentStatus::Done if integrating => "merging",
        AgentStatus::Done if run.has_merge_conflict() => "finished, merge conflict",
        AgentStatus::Done if agent_awaits_merge(run) && run.node_id.is_some() => {
            "finished, merges automatically"
        }
        AgentStatus::Done if agent_awaits_merge(run) => "finished, ready for review",
        AgentStatus::Done => "idle at its prompt",
        AgentStatus::Paused if run.parked.is_some() => "asleep",
        AgentStatus::Paused => "paused",
        other => other.as_str(),
    }
}

fn sync_blocked(run: &AgentRun) -> Option<String> {
    if run.needs_permission {
        return Some("a permission prompt".to_string());
    }
    match &run.wait_signal {
        Some(WaitSignal::Error(detail)) => {
            return Some(format!(
                "its last turn failed ({})",
                one_line(detail.as_deref().unwrap_or("api error"), 60)
            ));
        }
        Some(WaitSignal::Interrupted) => return Some("the user interrupted its turn".to_string()),
        Some(WaitSignal::Input) => return Some("a question for the user".to_string()),
        _ => {}
    }
    if run.has_merge_conflict() && !run.merge_conflict_files.is_empty() {
        let files = run
            .merge_conflict_files
            .iter()
            .take(3)
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        let more = run.merge_conflict_files.len().saturating_sub(3);
        return Some(if more > 0 {
            format!("a merge conflict in {files} and {more} more")
        } else {
            format!("a merge conflict in {files}")
        });
    }
    None
}

fn render_entries(entries: &[SyncEntry], you: usize, brief: bool) -> Vec<String> {
    let (task_max, latest_max) = if brief { (70, 80) } else { (110, 150) };
    entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            if index == you {
                // It knows its own task; one line says where it sits.
                return format!("- {} (you) · {} · {}", entry.name, entry.backend, entry.status);
            }
            let mut block = format!("- {} · {} · {}", entry.name, entry.backend, entry.status);
            if let Some(task) = &entry.task {
                block.push_str(&format!("\n  task: {}", truncate_chars(task, task_max)));
            }
            if let Some(latest) = &entry.latest {
                block.push_str(&format!("\n  latest: {}", truncate_chars(latest, latest_max)));
            }
            if !brief {
                if let Some(changes) = &entry.changes {
                    block.push_str(&format!("\n  changes: {changes}"));
                }
            }
            if let Some(blocked) = &entry.blocked {
                block.push_str(&format!("\n  blocked on: {blocked}"));
            }
            block
        })
        .collect()
}

/// The message one agent receives. Entry `you` is the recipient's own. Kept
/// under `SYNC_MESSAGE_CAP`: the brief form drops changes and cuts the text
/// fields shorter; past that, whole entries are left out and counted.
pub(crate) fn render_sync_message(entries: &[SyncEntry], you: usize) -> String {
    let join = |blocks: &[String]| format!("{SYNC_PREAMBLE}\n{}", blocks.join("\n"));
    let full = join(&render_entries(entries, you, false));
    if full.chars().count() <= SYNC_MESSAGE_CAP {
        return full;
    }
    let brief = render_entries(entries, you, true);
    let message = join(&brief);
    if message.chars().count() <= SYNC_MESSAGE_CAP {
        return message;
    }
    const MORE_RESERVE: usize = 40;
    let mut out = SYNC_PREAMBLE.to_string();
    let mut shown = 0;
    for block in &brief {
        if out.chars().count() + 1 + block.chars().count() > SYNC_MESSAGE_CAP - MORE_RESERVE {
            break;
        }
        out.push('\n');
        out.push_str(block);
        shown += 1;
    }
    let hidden = brief.len() - shown;
    if hidden > 0 {
        out.push_str(&format!(
            "\n- and {hidden} more agent{} not shown",
            if hidden == 1 { "" } else { "s" }
        ));
    }
    out
}

impl App {
    /// Indices of the rows `/sync` reports on, in list order.
    pub(crate) fn sync_participants(&self) -> Vec<usize> {
        self.agents
            .iter()
            .enumerate()
            .filter(|(_, run)| sync_participant(run) && !self.launch_preparing(&run.id))
            .map(|(index, _)| index)
            .collect()
    }

    fn sync_entry(&self, index: usize) -> SyncEntry {
        let run = &self.agents[index];
        let title = if run.task_summary.trim().is_empty() {
            summarize_task(&run.task)
        } else {
            run.task_summary.clone()
        };
        let title = one_line(&title, 60);
        let shared_checkout = run.is_main() || run.is_oneoff();
        let name = if run.is_main() {
            "main agent (shared checkout)".to_string()
        } else if shared_checkout {
            format!("{title} (shared checkout)")
        } else if let Some(node) = run.node_id.as_deref() {
            format!("{node} {title}")
        } else {
            title.clone()
        };
        let task = one_line(&run.task, 200);
        let task = (!task.is_empty()
            && task != "main branch"
            && !task.eq_ignore_ascii_case(&title))
        .then_some(task);
        let pane = run
            .terminal
            .as_ref()
            .and_then(|terminal| pane_latest_line(run.backend, &terminal.visible_lines_snapshot()));
        let summary = run
            .done_summary
            .as_deref()
            .map(str::trim)
            .filter(|summary| !summary.is_empty())
            .map(str::to_string);
        // Running: what it is doing now beats the last turn's report.
        let latest = if run.status == AgentStatus::Running {
            pane.or(summary)
        } else {
            summary.or(pane)
        };
        // The sidebar's cached diff stat; never computed here. The shared
        // checkout's diff is everyone's, so it is not this agent's.
        let changes = (!shared_checkout)
            .then(|| self.diff_summary_cache.get(&run.id))
            .flatten()
            .and_then(|(_, stat)| stat.as_deref())
            .map(short_diff_stat);
        SyncEntry {
            name,
            backend: run.backend.as_str().to_string(),
            status: sync_status(run).to_string(),
            task,
            latest: latest.map(|text| one_line(&text, 200)),
            changes,
            blocked: sync_blocked(run),
        }
    }

    /// The sync message for `recipient`, built from the current state. None
    /// when there is nobody else to tell it about.
    pub(crate) fn sync_message_for(&self, recipient: usize) -> Option<String> {
        let participants = self.sync_participants();
        let you = participants.iter().position(|index| *index == recipient)?;
        if participants.len() < 2 {
            return None;
        }
        let entries = participants
            .iter()
            .map(|index| self.sync_entry(*index))
            .collect::<Vec<_>>();
        Some(render_sync_message(&entries, you))
    }

    /// Whether a sync may be typed into this row at all. Rows with no pane are
    /// asleep (waking one for context would spend a whole resume); a `/gam`
    /// pair, a resolver or a merge owns its row's turns; a planned node about to
    /// merge on its own is finished; managed protocol rows end turns outside
    /// the hook path that keeps the reply quiet.
    fn sync_recipient_eligible(&self, index: usize) -> bool {
        let Some(run) = self.agents.get(index) else {
            return false;
        };
        let Some(terminal) = run.terminal.as_ref() else {
            return false;
        };
        sync_participant(run)
            && !self.launch_preparing(&run.id)
            && run.gam.is_none()
            && !run.merge_resolver
            && !run.has_merge_conflict()
            && run.integration.phase != IntegrationPhase::Integrating
            && !matches!(
                terminal.execution_profile(),
                Some("claude-sdk" | "codex-app-server")
            )
            && !self.is_auto_integration_candidate(run)
    }

    /// Idle at its prompt: the turn ended, nothing waits on the user, nothing
    /// was typed into its composer since (a draft or a local command may sit
    /// there), its finished turn's summary was already read, and no menu or
    /// prompt owns the keyboard.
    fn sync_recipient_ready(&mut self, index: usize) -> bool {
        let Some(run) = self.agents.get(index) else {
            return false;
        };
        let summary_read = !(run.node_id.is_some() && run.mode == AgentMode::Execute)
            || self.followups_ingested.contains(&run.id);
        let idle = run.status == AgentStatus::Done
            && run.wait_signal.is_none()
            && !run.needs_permission
            && !run.needs_user_input
            && !post_completion_output_is_new_turn(run.last_worker_input_at, run.completed_at)
            && summary_read;
        idle && self.composer_blocked_at(index).is_none()
    }

    /// `/sync`: queue the summary for every agent that can take it and send it
    /// now to the ones already idle at their prompt.
    pub(crate) fn start_agent_sync(&mut self) {
        let participants = self.sync_participants();
        if participants.len() < 2 {
            self.notice = Some(
                "nothing to sync: /sync needs at least two running or finished agents".to_string(),
            );
            self.dirty = true;
            return;
        }
        let mut queued = Vec::new();
        let mut skipped = 0usize;
        for &index in &participants {
            if !self.sync_recipient_eligible(index) {
                skipped += 1;
                continue;
            }
            let run = &self.agents[index];
            let Some(instance_id) = run.terminal.as_ref().map(|t| t.instance_id().to_string())
            else {
                skipped += 1;
                continue;
            };
            let run_id = run.id.clone();
            // A second /sync replaces the first; the message is built when it
            // goes, so a waiting agent never gets two.
            self.pending_syncs.retain(|pending| pending.run_id != run_id);
            self.pending_syncs.push(PendingSync {
                run_id: run_id.clone(),
                instance_id,
                queued_at: Instant::now(),
            });
            queued.push(run_id);
        }
        let started = Instant::now();
        self.flush_pending_syncs();
        let busy = queued
            .iter()
            .filter(|id| self.pending_syncs.iter().any(|pending| &pending.run_id == *id))
            .count();
        let sent = queued
            .iter()
            .filter(|id| {
                self.sync_deliveries
                    .get(id.as_str())
                    .is_some_and(|delivery| delivery.at >= started)
            })
            .count();
        // Queued but dropped by the first flush: its pane had just exited.
        skipped += queued.len().saturating_sub(busy + sent);
        let reached = sent + busy;
        let mut notice = if reached == 0 {
            format!(
                "nothing sent: none of the {} agents can take a message now (asleep or merging)",
                participants.len()
            )
        } else {
            let mut text = format!("synced {reached} agent{}", if reached == 1 { "" } else { "s" });
            if busy > 0 {
                text.push_str(&format!(" ({busy} busy, they get it when their turn ends)"));
            }
            text
        };
        if skipped > 0 && reached > 0 {
            notice.push_str(&format!("; {skipped} not sent (asleep or merging)"));
        }
        self.record_activity(format!(
            "sync: shared what {} agents are doing; {sent} got it now, {busy} when their turn ends, {skipped} not sent",
            participants.len()
        ));
        self.notice = Some(notice);
        self.dirty = true;
    }

    /// Deliver every queued sync whose agent is idle at its prompt now. Rows
    /// that left the picture (merged, stopped, deleted, restarted, about to
    /// merge) and syncs older than `SYNC_PENDING_TTL` are dropped. Called every
    /// poll tick; free when nothing is queued.
    pub(crate) fn flush_pending_syncs(&mut self) {
        if self.pending_syncs.is_empty() {
            return;
        }
        let mut waiting = Vec::new();
        for pending in std::mem::take(&mut self.pending_syncs) {
            let Some(index) = self.agents.iter().position(|run| run.id == pending.run_id) else {
                continue;
            };
            let same_pane = self.agents[index]
                .terminal
                .as_mut()
                .is_some_and(|terminal| {
                    terminal.instance_id() == pending.instance_id && terminal.is_alive()
                });
            if !same_pane
                || pending.queued_at.elapsed() >= SYNC_PENDING_TTL
                || !self.sync_recipient_eligible(index)
            {
                continue;
            }
            if !self.sync_recipient_ready(index) {
                waiting.push(pending);
                continue;
            }
            self.deliver_sync_to(index);
        }
        waiting.append(&mut self.pending_syncs);
        self.pending_syncs = waiting;
    }

    /// Type the sync into one agent's pane. Context, not a follow-up: no turn
    /// is recorded, and nothing the user's own prompts change is touched.
    pub(crate) fn deliver_sync_to(&mut self, index: usize) -> bool {
        let Some(message) = self.sync_message_for(index) else {
            return false;
        };
        if !self.live_inject_at(index, &message) {
            return false;
        }
        let run = &self.agents[index];
        let run_id = run.id.clone();
        if run.is_main() || run.is_oneoff() {
            // The finished turn's summary was read when it ended; the reply to
            // this sync must not replace it.
            self.direct_summary_read
                .insert((run_id.clone(), run.turns.len()));
        }
        self.sync_deliveries.insert(
            run_id,
            SyncDelivery {
                at: Instant::now(),
                reply_open: true,
            },
        );
        true
    }

    /// True when the newest input to this run was a sync, so it is not the
    /// user having typed into it recently.
    pub(crate) fn sync_was_last_input(&self, run: &AgentRun) -> bool {
        self.sync_deliveries
            .get(&run.id)
            .is_some_and(|delivery| only_sync_input_since(run, delivery.at))
    }
}
