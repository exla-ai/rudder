# Rudder v4 handoff (2026-11-04)

Claude Code and Codex limits on Cloud homes keep the account signed in and
show the reset in Settings. New work and the next prompt in a saved,
idle conversation use the first available configured account when the
current account is confirmed exhausted. Each account keeps its own sign-in.

Agent rows on the home and project pages render compact Markdown previews
with readable link labels, emphasis and inline code.

How to work: see "Development speed" in `AGENTS.md` (main only, targeted
checks, CI only at release, no new status docs, no hardening waves).

This is the single entry point for whoever picks up v4 next. It reconciles
the status of every agent from the parallel plan `plan-20181-0`, says which
doc owns what, and lists what is open. Nothing is running right now: all
workers are merged or cancelled, the planner run is deleted, and the
hardening follow-up queue was emptied (backup:
`/workspace/rudder-build/hardening-queue-backup.json`).

4.2.12 is published on npm and deployed to Cloud. Worker images are available
for Fly and external homes; active homes adopt them when idle. The recovered
cloud-typing work is integrated, and the three obsolete Git worktrees are removed.

4.3.0 is published on npm and deployed to Cloud, with worker images for Fly
and external homes. Context usage appears only at the bottom of the agent
sidebar; individual cards have no context text. Active homes adopt the image
when idle. The root-owned CLI in a home updates with its image, rather than
through an in-place global npm install.

4.3.2 is published as npm latest and deployed to Cloud (release 200). Both
Fly worker registries serve the same image; app and health checks passed.
Release CI passed all qualification checks. The context panel is compact,
subagent trees show only a fold glyph without counts, and drawer headers
omit expand/collapse instructions. Active homes adopt the image when idle.

Local UI cleanup: removed sidebar context/time charts and delivery proof/status
clutter from agent cards. Agent list uses the full sidebar height.

Local model tracking: an agent's title and card follow the model its session
log last answered with, so `/model` inside Claude Code or Codex shows up after
the next reply and is kept on resume. Prices cover Fable 5.2, Opus 5.5, Sonnet
5.5 and Haiku 5.5; titles use the `haiku` alias and planning uses
`claude-sonnet-5-5`.

4.3.3 is published as npm latest and deployed to Cloud (release 201,
2026-11-10 03:42Z) with this and the sidebar cleanup above. Both Fly worker
registries and ghcr serve the 4.3.3 image; health checks passed. Active homes
adopt the image when idle.

Agent pins: `p` or clicking the title pin control toggles a persistent pin.
Pinned agents stay above the regular list in both views, including finished runs.
Pinning also works from the worker pane with `Ctrl+W`, then `p`, or `p` in
`Ctrl+G` navigation mode. These command routes previously ignored the pin key.

4.3.5 is published as npm latest and deployed to Cloud (release 202,
2026-11-10 05:22Z). It includes persistent agent pins and removes the old
expansion hint. Release CI passed; both Fly worker registries serve the same
image, and app and health checks passed. Libra's existing dashboard was still
running 4.3.0, which had neither change. Active homes adopt the new image when
idle; live agents were not restarted.

4.3.6 pin follow-up: pinned conversations have a named section above main and
the status sections in both views. Pins prevent idle parking, finished-row
cleanup, automatic node review and automatic merging. A live pin between replies
says it is ready for the next message and no longer announces ready for review.
Cloud cleanup inventory exposes pins and protects their child processes too.
Checked with the native `agent_pin` filter and `tests/cloud-cleanup.test.mjs`.
4.3.6 is published as npm latest and deployed to Cloud (release 203,
2026-11-10 10:12Z). Full local release checks and release CI passed. Both Fly
worker registries serve the same 4.3.6 image, the ghcr workflow passed, and
app and health checks returned 200. Libra's live home still runs 4.3.0 but now
offers the 4.3.6 image; it will adopt when idle. Live agents were not restarted.

4.4.0 pin and resume fixes: unpinned rows no longer show a literal `p` before
the title (only pinned rows show the pin icon, which unpins on click). Enter in an
empty task box now opens the selected agent like Enter in the list: a live pane
takes the keys and a stopped one resumes. Clicks keep focus in the task box, so
that Enter used to do nothing and agents looked unreachable after a home update.
4.4.0 Cloud (2026-11-10 23:00Z): the worker image was built on the home host
as 4.3.6 plus the locally built 4.4.0 `rudder-native` and package version (only
those changed), pushed to rudder-worker-images:4.4.0 and copied to
rudder-workers-exla:4.4.0 (digest 843d727b). The control plane was redeployed
on its 4.3.6 image with the new RUDDER_WORKER_IMAGE. The home offers the update.

4.4.1 fork relaunch fix: a lower materialized by `ensure_lower` (engine
jj_working_copy) had no `.jj`, so once a snapshot repointed a fork at one, the
fork's enter script failed `mount --bind ... $M/.jj` (exit 32) and the agent
closed the moment it reopened (cedar in libra after the 4.4.0 restart). Lowers
now get an empty `.jj` (existing ones on reuse) and the enter script creates the
mount point if missing. The home's 19 existing lowers were fixed by hand.
4.4.1 Cloud (23:39Z): same overlay build on the host (4.4.0 plus the new
rudder-native, rudder-jj and dist/warm-snapshot.js), rudder-worker-images and
rudder-workers-exla:4.4.1 (digest 902a3630), control plane redeployed with the
new RUDDER_WORKER_IMAGE. The home offers 4.4.1; npm comes from the v4.4.1 tag run.

4.4.2: god's view switches tiles with Alt+Tab / Alt+Shift+Tab and Alt+1-9
(tiles are numbered); `morph/kimi` and `morph/deepseek` run Claude Code on
Morph like kimi/glm on OpenRouter; the home pings its Cloud links and redials
any silent for 45 s. Before that, 19 of home-53c2's 23 links had died inside
the proxy without a close (silent up to 47 hours), so laptops sat on
"Reconnecting cloud input"; those sockets were killed by hand on 2026-11-13
and redialed. 4.4.2 Cloud (23:39Z): overlay of 4.4.1 plus the new
rudder-native and home.mjs, rudder-worker-images and rudder-workers-exla:4.4.2
(digest 3da31bc7), control plane redeployed. The home offers 4.4.2; npm comes
from the v4.4.2 tag run.

5.0 dashboard matches the app (shipped in 5.0.4): the agent list uses the app's
sections and words (`needs you` first, then `working`, `done`, and the `merged`
and `earlier` drawers; "ready for review", "turn failed", "merge conflict"),
the header counts working and need you, `P` switches the selected agent's
model and effort through the app's model steer, the worker border shows tok/s
and context left like a chat header, and the palette is the app's. Checked
with native filters `agent_stats`, `needs_you_groups`, `model_key_switches`,
the updated sidebar/label tests, and `tests/tui/done-drawer.test.mjs`.

Vibeprint canvas in the app (shipped in 5.0.4): a Canvas button beside Files
opens the project's vibeprint canvas in the right column (a sheet on phones),
following the selected agent. The home worker runs one `vibeprint serve` per
project (`cloud/worker/vibeprint-host.mjs`, RPC `vibeprint`, Cloud route
`/api/app/vibeprint`) with state in `~/.rudder/vibeprint/<workspace>`, never
in the checkout, and no vibeprint rule or MCP for agents. The frame is the
relay page signed in by a `#t=` token; "Send to agent" previews the canvas
brief and sends it as a normal message. Build tracking: the panel's eye (and
Send to agent) tracks an agent; after its turns the home sends its `jj diff`
to vibeprint's `POST /api/track` (`cloud/worker/vibeprint-track.mjs`): no pass
for an unchanged diff, at most one per agent per 10 minutes. vibeprint's own
"Update from code" checks the checkout on the same runner, and without an API
key the local Claude Code or Codex reads submitted sketches. Needs vibeprint
0.10.0 on npm (commit 2fdd8fb2 in that repo, not pushed) before the worker
image builds. Checked with `tests/vibeprint-host.test.mjs` and headless runs
against dev-mock with a fake Claude Code.

Agent rename, row menu and day-old cleanup (shipped in 5.0.4): the app renames
an agent in place (double click a sidebar title, the row menu, or click the
agent screen's title) through `/api/app/agent-rename`, RPC `agentRename`, steer
kind `rename`, native `rename_agent_at` (the dashboard's `r` uses it too, 200
characters at most). Desktop rows have a right-click menu (also the
context-menu key and Shift+F10): Rename, Pin/Unpin (`/api/app/action` pin and
unpin, steer kind `pin`), Open beside, Close (the existing delete). The native
dashboard deletes agents nobody touched for 24 hours (`agent_life.rs`; replaces
the 7-day finished-agent sweep): newest of run.json mtime, current transcript
mtime and the jj working-copy commit time, checked every 15 minutes off the
App loop. The clock starts at the first scan in a project
(`.rudder/cleanup-since`), so nothing is deleted on update; existing agents
get a full day from then. Never collected: pinned, running or waiting, pending asks, cloud
owned, mid-integration, unmerged plan nodes, orchestrators of a live plan,
finished work waiting for a merge, merged work that is no longer in main, and
any agent whose workspace has edits main does not have, whatever its status.
Every delete (`dd`, `cc`, the app, the cleanup) now deletes the conversations
Rudder made for the agent and no other (`session.rudderSessionIds`: launches
with `--session-id`, edit and branch copies, Codex forks, the Codex session a
workspace agent's launch started): Claude sessions with their folder, and Codex
rollouts no remaining run's `history_base` chain needs. A conversation the agent
was switched to (`/resume`, a restored session) stays. Workspace removal
snapshots first, so the agent's jj change stays in `jj log`; a workspace with
no jj name is kept. Checked with native filter `untouched_agents` and
a headless run against dev-mock.

Moving agents in the app's sidebar (not released yet): drag an agent row up
or down within its project (touch: press and hold first, so a swipe still
scrolls; hold and let go opens the row menu), Alt+Up / Alt+Down on a focused
row, or Move up / Move down in the row menu. The row lifts, the others slide
around a gap, the sidebar scrolls near its edges, and Esc or letting go outside
its project puts it back. The order is saved per project on the Cloud row
(`rudder_workspaces.agent_order`, `POST /api/app/agent-order`, `agentOrder` in
`/api/app/state`), so every device shows it; the phone's project screen keeps
it within each status group. Agents not in the saved order (all of them until
the first move, new ones after) sit on top, newest first, as before. Pins and
"Needs you" do not change it. Agent rows no longer use HTML drag and drop:
dropping a row on the main area to open it there (beside the agent on screen)
is part of the same pointer drag. Checked headless against dev-mock (mouse,
touch, keyboard, menu, cross-project refusal, reload, second context).

Every run is jj (shipped in 5.0.4). The git-worktree path is gone: the
`vcs ?? "git"` routing and the git merge, sync and rebase helpers in `git.ts`,
the `worktree` record key, `workspace.branch`, the `mergeStrategy` setting, the
cloud handoff's `worktree*` spellings, native's git rebase conflict prompt, and
the record's `vcs` field (an old record that still has it loads like any other).

Branch an agent (shipped in 5.0.4): the app's row menu (Branch, with an
optional first message) and the dashboard's `b` start a new agent named
"<name> (branch)" right after the source, with a copy of its conversation and
files, while the source keeps running (`native/src/branch.rs`; route
`/api/app/agent-branch`, RPC `agentBranch`, steer kind `branch` with
instruction `branch` or `branch\n<prompt>`, answered when the branch starts).
Files: `jj duplicate @` in the source's workspace snapshots it and makes a
sibling copy of its working-copy commit, so the source's later edits never
reach the branch; the branch's workspace starts on the copy. Claude: the whole
session file goes into the branch's project folder under a new id, which is
the branch's session and is resumed directly (`--resume <copy>`, as an edited
chat is), so its chat shows the history before its first message. It used to
resume with `--fork-session --session-id`, whose session Claude writes only on
the first message: a branch started without one showed an empty chat in the
app. Codex: `thread/fork` of the whole thread on a one-shot app-server, cwd
the branch's workspace, goal continuation deferred. Without a prompt the
branch is Done, idle at its prompt. The cleanup keeps the source's
conversation while a branch of it is being set up. Checked with native filter
`branch` and for real (11-19): a scratch dashboard with a Claude haiku and a
Codex luna agent, branched through the steer inbox with and without a prompt.
A 4.4.2 home answers "unknown method agentBranch"; Cloud now says to update it.

Edit any sent message (not released yet): Edit under a message in the app
turns it into a text box in place (Enter sends on a desktop, Shift+Enter is a
new line, Esc cancels; the turns after it fade). Every turn-starting prompt is
editable, the first one too (`editablePrompts` in `session-lineage.mjs`).
Steer kind `edit`, `<entry> <latest>\n<text>`, handled in
`native/src/chat_edit.rs`. An earlier prompt rewinds (Claude: a prefix copy of
the session resumed; Codex: `thread/fork` before that turn). The first prompt
starts over in the same agent and workspace: Claude `--session-id <new>`, Codex
a new thread (`thread/start` plus `thread/name/set`, which is what makes Codex
save it), with the edited text as the task in the framing it launched with
(goal, discussion scope). Mid-turn, the edit first writes Esc (the app's Stop)
and waits up to 20 seconds for the turn to end; a row whose last turn ended on
an API error (StopFailure: rate limit, refusal) is not mid-turn. An edit the
home answers "queued" says "Sending the edited message" on the message until
its receipt comes with the transcript; a failed one is a toast with the
reason. Prompts on pages loaded with "Load earlier messages" are editable too,
and a prompt written twice around an auto-compaction is one edit. The chat shows a discussion's
or main agent's first message as the user's text. Checked with native filter
`chat_edit` and for real (11-19): a scratch dashboard with a Claude haiku and
a Codex luna agent, first-message and mid-turn edits; both forgot the dropped
turns.

All agents (shipped in 5.0.4): a button at the top of the app's sidebar
(phone: the home top bar) opens one chat that talks to every agent in every
project on the home: list, read, message, interrupt, start, answer prompts and
watch for finished/failed/needs-you without polling. It is a Main agent in a
hidden local project `rudder-all-agents` with an `all_agents` MCP server
registered for that project only; every action goes through the owning
project's steer inbox (engineering reference 9.8.2, "All agents"). The +
button in the chat starts a new one; older chats are reachable only until the
24-hour cleanup deletes them. Checked with `tests/coordinator.test.mjs`, a real
`claude -p --model haiku` run against the broker with fake projects (all five
read/write/wait tools), `codex mcp list` for the project config, and headless
screenshots against dev-mock.
On main since: the chat starts with its own short framing (what it is, its
tools, that it edits nothing) instead of the checkout owner's rules: home.mjs
starts its dashboard with `RUDDER_ALL_AGENTS=1` and native
`all_agents_task_prompt` builds the first prompt. A new Codex chat shows its
first reply within a second instead of after up to a minute: the Codex
session was recorded only on the dashboard's 60s reconcile tick, and now the
1s rollout scan pins the thread the worker's own SessionStart hook names. The
worker also finds a rollout written after a remembered miss in today's day
folders. Checked for real (11-19) with a scratch dashboard and Codex luna:
first reply to the worker's transcript 42s and 22s before, 0.8s and 0.5s
after.

"/" menu (shipped in 5.0.4): Claude's list comes from Claude itself. The
worker opens an interactive claude off-screen in a private tmux with the
agent's env, cwd and model, types "/" and parses the popup (nothing is sent,
no transcript; ~4s, cached on the Claude version, model and every file that
changes the menu). Matched the real popup row for row on Claude 2.1.327 (198
rows) and Codex 0.169/0.170 (56). Each build starts the user's MCP servers,
as an agent start does. Not run on macOS yet. opencode agents still get
"Commands unavailable" (the app starts only Claude and Codex agents).
On main since: the menu never waits on a build. Each device keeps the last
10 agents' lists (an agent not opened there yet shows the newest list of its
CLI) and swaps in the home's answer; the home saves its lists to
`<volume>/home/.rudder/slash-commands.json`, builds a new agent's list within
5s, and answers a first build with the popup's first reading (~1s, MCP
prompts may still be joining, `settling: true`) before the full list (~4s).
Claude's headless `initialize` list (what t3code uses) is not enough on its
own: it lacks 53 TUI built-ins, every MCP prompt and 7 bundled skills.

5.0.4 is published as npm latest (2026-11-18 13:56Z) and deployed to Cloud
(Fly release 209, 14:01Z, worker image `rudder-workers-exla:5.0.4`, also in
`rudder-worker-images`). Full local release checks and release CI passed. 5.0.2
and 5.0.3 never published: each stopped in Release CI's macOS native tests on a
test that was timing-sensitive under load (the fleet test's provider check, then
the gam fake reviewer printing nothing between its prompt hook and turn end).
Release CI accepts only the first attempt of a tag, so each fix took a new
version. The site (rudder.exla.ai, Linear removed) is deployed. The live home
offers the image and adopts it when idle. The Mac app is not released: the
download still serves 1.0.2; a signed build needs the Developer ID identity
(the MacBook Pro, or a Developer ID Application .p12 as the
MAC_DEVELOPER_ID_P12 and MAC_DEVELOPER_ID_PASSWORD secrets for mac.yml).

5.0.13 is published to npm (Release CI passed 2026-11-19 10:49Z) and deployed
to Cloud (Fly release 226 at 10:33Z, worker image `rudder-workers-exla:5.0.13`
pinned, built with `worker-image.mjs --compile`, copied to
`rudder-worker-images`). 5.0.12 never reached npm: Release CI's macOS
qualification failed two steer_watch tests, a kqueue race (a directory deleted
between open and registration was watched as a dead vnode; 17 of 30 runs
failed on an Offstage Mac, 0 of 130 after the fix). Cloud ran the 5.0.12 image
meanwhile (releases 224 and 225). The Mac app 5.0.13 (5000013) is published as
`mac-v5.0.13-5000013` (21:26Z): the unsigned app was built on an Offstage Mac,
and the user signed, notarized and published it on the MacBook Pro by running
one script (`mac/scripts/package.sh`, then `publish.sh`) because the laptop's
Tailscale stopped answering. Release secrets still lack the Developer ID .p12,
so Mac releases still need that laptop. In 5.0.12 and 5.0.13: OpenCode as a full
harness (opencode-ai 1.19.5 in the image); one right-side panel for Files,
Canvas, Notes, the agent's terminal (xterm.js, read only) and file links;
notes per agent (`.rudder/runs/<id>/notes.md`, replacing RUDDER_NOTES.md);
file paths in messages and tool cards open in the viewer (the home reads them
for the owner only, with a deny list); Goal active in the chat header;
teammate and subagent messages as From cards; compaction as a folded card
with sizes, duration and summary, live while Claude compacts (run.json
`compactingSince` from the pane), and Compact now in the meter popover; the
send check, busy, idle, interrupt, ready and question detectors read the
live rows above the composer (Claude 2.1.328 draws an agent list under its
footer); the Mac find bar floats (ships with the next Mac build). Open:
Antigravity is on a branch waiting for its sign-in code; the GitHub App must
be made public by its owner for other accounts to sign in; an interactive
shell for the terminal panel is not built; the Mac and CLI GitHub sign-in
pass no error callback URL.

5.0.11 is published as npm latest and deployed to Cloud (web app Fly release
215 at 2026-11-19 06:15Z, worker image `rudder-workers-exla:5.0.11` pinned in
release 216 at 06:27Z, built with `worker-image.mjs --compile`). Rudder for Mac
5.0.11 (5000011) is the latest Mac release (`mac-v5.0.11-5000011`). In it:
moving a chat to another harness (same row, a handoff built from the
transcript with no model call, optional summary instructions), the subagent
tray, Cmd+K command palette (the terminal button is gone), quote reply, model
chips with harness logos and "Sol 6.1" style names in per-harness sections,
right-click a project for a new chat, the All agents update screen, the image
message bug, dashboard app actions within milliseconds (steer inbox watched
with inotify/kqueue instead of read once a second), and tests that never start
the real claude. Open: Antigravity CLI support is built in a worktree but not
run against a signed-in agy; moving to a harness that is not signed in is not
refused yet; slash commands in reply mode get the quote; inline teammate cards
skip the membership rule; tests leak about 29 temp dirs per run.

5.0.10 is published as npm latest and deployed to Cloud (Fly release 214,
2026-11-19 04:12Z, worker image `rudder-workers-exla:5.0.10`, built from the
tag's source with `worker-image.mjs --compile`). Rudder for Mac 5.0.10
(5000010) is the latest Mac release (`mac-v5.0.10-5000010`): the Mac version
now follows Rudder's (`npm version` runs `scripts/mac-version.mjs`), and
`mac-publish.yml` signs its appcast with the repo secret. 5.0.6 to 5.0.9 never
reached npm (Release CI on macOS: a PTY test racing `stty raw`, and
`kill(-group, 0)` answering EPERM for a group of exiting processes); Cloud ran
5.0.8 and 5.0.9 images meanwhile. In it: Notes (RUDDER_NOTES.md), Files for
agents' own workspaces, edit any message in place (also mid-turn and the
first one), Close removes the row at once, drag to reorder agents, a faster
"/" menu, All agents speed and framing, a new Codex chat shows its first reply
within a second, the Codex permission false positive, vibeprint 1.0.0, and on
the Mac Cmd+F, a new About window and a DMG whose Applications item shows its
icon. Open: older chat messages past the last 300 are out of reach (find and
scroll), and the dashboard reads its steer inbox once a second.

5.0.5 was published as npm latest and deployed to Cloud (Fly release 210,
2026-11-18 15:28Z, worker image `rudder-workers-exla:5.0.5`, also in
`rudder-worker-images`). It qualified first on a `release-check/5.0.5` branch
and the tag reused that run. Fixes from a real-Mac run of 5.0.4: a merge lands
on the main bookmark (no empty commit, Git left on main) and moves the agent's
workspace off the landed change; the second `m` in the diff panel merges; a
dismissed Codex plan stops reading "needs input" (Plan mode left, or the menu
gone from the screen); refusals show as errors; paused and merged rows keep
their +N -M. The "/" menu probe now waits for Claude's process group to exit.
Open: the phone card for a Codex plan dismissed with Esc only, a Codex-only Mac
defaults to Claude. Fixed on main since: the scheduler's node merge
(`mergeNodeIntoIntegration`) lands on main through the dashboard's merge, and
plan nodes start from main instead of the checkout's `@`.

Rudder for Mac 1.1.0 (4) is the latest Mac release (`mac-v1.1.0-4`, 2026-11-18),
Developer ID signed, notarized and stapled; `/download/mac` serves it. Xcode
hung on the MacBook (macOS pipe buffers shrunk to 512 bytes after 12 days up,
so the build service deadlocks on clang's probe), so an offstage Mac built the
unsigned Release archive and the MacBook signed, notarized and packaged it.

The Mac app updates itself from the next Mac release, 1.2.0 (not released
yet): Sparkle 2.10.1, a daily check that downloads in the background and
installs on quit, and Rudder > Check for Updates…. The feed is `appcast.xml`
on the latest GitHub release. `mac/scripts/publish.sh` stages a draft
release and runs `.github/workflows/mac-publish.yml`, which EdDSA-signs the
feed with the `SPARKLE_ED_PRIVATE_KEY` repo secret, verifies it against the
app's `SUPublicEDKey` and publishes; `mac/scripts/package.sh` signs Sparkle's
helpers. Open: a backup of the key (the secret is the only copy since a
machine update deleted `~/.rudder-sparkle-ed25519` on 2026-11-19), and a
server-side web app version so the Mac app can reload after a Cloud deploy.

Tailscale is a step in the app (Settings and the Get set up card), on main and
not released yet. Connect starts `tailscaled` on the home when it is not
running and runs `tailscale up` with no auth key; the app opens the
login.tailscale.com link it prints and the step turns connected once status
says Running (`cloud/worker/tailscale-login.mjs`, `cloud/app/tailscale-step.js`).
A tailnet that approves new devices shows Needs approval until an admin does.
Disconnect is `tailscale logout`. The node key stays on the volume, so a
restart rejoins with no new login. TS_AUTHKEY is no longer read. Homes get it
with the next worker image.

OpenCode (`opencode`, pinned at 1.19.5 in the worker image) is a full agent
harness: the dashboard, the CLI, Cloud homes and the phone app start, resume,
fork and read it, and chats move to and from it. Rudder's plugin
(`native/src/opencode_plugin.mjs`, loaded through `OPENCODE_CONFIG`) reports
turns, the session id, permissions and questions, answers the phone's asks
over OpenCode's HTTP API, and sends a first message into a session the TUI
opens (1.19 ignores `--prompt` with `--session`). An effort is the model's
variant, which the plugin sets on each of the run's messages (the TUI
otherwise reuses the variant last used with the model). Transcripts, usage and context
are read from OpenCode's SQLite database. The phone signs OpenCode's OpenAI
provider in with ChatGPT's device code; with no sign-in it runs OpenCode Zen's
free models. Open: OpenCode subagents in the app's tray, the app's "/" list
for it, and token use in the dashboard's timeline.

Antigravity (`agy`, pinned at 1.5.14 in the worker image) is a fourth agent
backend. The dashboard, the CLI, Cloud homes and the phone app can start,
resume and read it. Its turn state comes from hooks in
`~/.gemini/config/hooks.json` (a `rudder` entry that does nothing unless
`RUDDER_AGY_HOOK` is set). Permission prompts and questions come from its OSC
7501 terminal status (`AGY_CLI_OSC7501=on`). Transcripts are read from
`~/.gemini/antigravity-cli/brain/<id>/.system_generated/logs/transcript.jsonl`,
and each agent's own workspace folder is trusted through `trustedWorkspaces`
in its `settings.json` (agy asks again in any subfolder of a trusted one, and
that dialog swallowed the first follow-up).
Rudder refuses with a plain reason where agy has no way in: fork, branch and
handoff (agy has no command-line fork), prompt edits, the gam reviewer (it has
no read-only sandbox) and peer MCP (agy reads MCP servers from one global
file only). The app's Settings agents list signs it in to Google with a
pasted code, and a chat can switch to and from it in the model picker. Its
models come from `agy --output-format json models`, which agy prints only
when signed in: a result envelope with the list under `command.data.models`
as `{id, label}` (tests/fixtures/agy-1.5.0-models.json). An id is a slug per
effort (`gemini-3.8-flash-high`), so the pickers offer no efforts or default
for it. Open: phone answers to its permission
prompts, the app's "/" list for it, the files an edit wrote in a handoff
from it (its CODE_ACTION step), and the files hub showing `~/.gemini`
instructions and skills.

Claude Code and Codex can hold several signed-in accounts (`src/accounts.ts`).
An account is a config dir: "main" is the CLI's own (`~/.claude`, `~/.codex`),
and each added one is `~/.rudder/accounts/<backend>/<id>`, which agents run on
with `CLAUDE_CONFIG_DIR` or `CODEX_HOME` set to it. An added dir keeps its own
sign-in and links only what `SHARED` names to the main dir (settings, skills,
hooks, memory files and the conversation logs), so transcripts, usage and
resume read one place and a chat can move between accounts. Claude 2.1.328 and
Codex 0.170.0 write through those links (checked with `auto-mode reset`,
`plugin install/disable`, `mcp add/remove` and `features enable`). The default
account of a backend is `backends.<backend>.account` in config.json. A run
keeps its account in run.json, and the model and harness steers name it.
Cloud homes sign in each account (`login/start` with `account`: main, an id or
"new"), and `/api/app/agent-account` makes one the default, signs it out or
removes it. Settings and Home group accounts under each agent: a row names
the account, its state, plan and usage, with Sign in or Sign out as a button
on it, and Make default and Remove under its "...". Each Claude Code or Codex
group has its own Add another account button, and the default is tagged. The
Antigravity row shows the Google account from the `applyAuthResult` line of
its own `agy --log-file <tmp> models` run (agy never logs its plan tier), and
"Use another Google account" signs out and reopens sign-in, since agy offers
its login only when signed out. Gemini 4 Neon needs a paid Google AI Pro or
Ultra account; a free account lists only Gemini 3.8 Flash and the partner
models. The Claude Code sign-in sheet has a full-width
field for pasting the sign-in code. The model pickers choose the account when
a harness has more than one. Plan usage is per account. On macOS, Claude's
keychain entry for an added dir is `Claude Code-credentials-<sha256(dir)[:8]>`.
Antigravity has one account because agy has no config dir setting. Open work
includes verifying a real sign-in into an added account on a Cloud home and on
macOS, resuming a Codex chat on another account, and adding accounts from the
Mac dashboard.

Mac release CI imports the existing Developer ID Application identity from
`MAC_DEVELOPER_ID_P12` and `MAC_DEVELOPER_ID_PASSWORD`. It archives with Xcode
signing disabled, signs Rudder and Sparkle with the same `sign-app.sh` used by
local packaging, and uses the App Store Connect key for notarization. The
current certificate expires on 2027-02-01. Renew it through the Apple Account
Holder and replace both signing secrets. The team API key cannot issue it.
CI published Mac 5.0.18 (5000018) on 2026-11-20 at 01:28 UTC. The public DMG
and the copied app passed Gatekeeper and stapler validation on macOS, and
the public Sparkle feed verified against the shipped key.
Mac release CI holds the signed installer until Cloud reports the worker image
pinned for that version and the hosted app responds.

Chat and subagent Markdown preserve `vnc://` link destinations and show the
address on hover. The macOS shell opens these links in Screen Sharing.
This change landed after the Mac 5.0.18 build.

On Mac, the tab strip appears when a second tab opens and hides again with
one tab. Cmd+T opens a tab while the strip is hidden. Desktop browsers keep
the strip visible to provide the + button for opening the first extra tab.
Drag a tab to reorder it.
The order is saved on the device and used by the tab cycling shortcuts.

Mac and iPhone Settings show a working Claude sign-in's upcoming expiry
separately from failed model checks. Antigravity's signed-out response is a
sign-in state. The worker checks terminal approval controls before blocking
chat delivery, so permission-related prose does not stop a reply.

The iPhone shell's foreground and pull-to-refresh bridge reloads state using
the token injected when the web view was created. The tutorial links directly
to the iPhone TestFlight invite.

The shared chat keeps queued sends visible until their transcript entries
land. A late dashboard failure gives the message a Retry button. Consecutive
sends to the same agent each get their own terminal submission. An automatic
compaction leaves a manually queued `/compact` pending.

OpenCode chats show subagent activity and discover project slash commands.
Resumed OpenCode conversations send compaction and project commands through
OpenCode after the session opens. Antigravity permission requests link to
Antigravity Remote Control when the CLI provides its address.

Keyboard navigation reveals a collapsed project or Needs You section when it
contains the destination agent. The Mac tab strip appears when Cmd+T opens a
second tab. Control+Tab and Control+Shift+Tab cycle through the open tabs.

Chat links open supported images and PDFs in the read-only file viewer. Bare
image filenames in inline code also link to the viewer.
The chat composer highlights `/goal` as you type it. Cloud home Codex chats
show the new objective with Replace current goal and Cancel while the terminal
confirmation is open.
Questions with no choices open directly to an answer field, and Enter submits
an answer from that field. Codex question answers stay visible as queued until
all terminal writes have delivery receipts. If the worker restarts mid-answer,
Rudder marks delivery unconfirmed and the terminal can be checked.

Mac local tool discovery runs when opening a local copy and refreshes the
login shell path on retry. iPhone recovery preserves the requested page or
digest notification link through an offline load or WebKit restart. The
shared app pauses state and chat polling in the background, compresses both
responses, and loads Markdown and syntax coloring where they are used. A
send refreshes its receipt even when an earlier chat poll is still running.

Cmd+F (Ctrl+F) in a chat opens the chat's own find (`cloud/app/chat-find.js`):
matches are highlighted with CSS highlights, stepping older loads earlier
pages, and other screens keep the browser's find. Cmd+Shift+F opens Search
everything (`cloud/app/global-search.js`): agents and projects by name from app
state, and messages from `GET /api/app/search`, which calls the worker's
`searchChats` (`cloud/worker/transcript-search.mjs`). It scans Claude, Codex
and Antigravity session files raw and parses only the lines that hit: user and
assistant text only, 50 results, 5 per chat, 4 seconds. An older worker shows
the update sheet. In the Mac app, Edit > Find opens the chat's find on a chat,
and Edit > Search Everything opens the panel.

## 1. Which doc is authoritative (locked 2026-11-04)

Only two docs steer work. Everything else is reference or evidence.

| Doc | Role |
|---|---|
| `PLAN.md` section 0 | **The locked direction.** Overrides the rest of PLAN.md. |
| `docs/v4-handoff.md` (this file) | **Current status and open work.** Update it, do not start new status docs. |
| `PLAN.md` sections 1-20 | Background design. Read the relevant section only. |
| `docs/engineering-reference.md` | How things work; release workflow. |
| `docs/architecture/v4-release-readiness.md` | E0-E14 gate detail (written before the scope cut). |
| `docs/architecture/*.md` (implementation, experiments, research and about 30 more) | Background reference. Do not extend. The JSON receipts and `examples/*/evidence` were deleted 2026-11-04; they are in git history before that date. |
| `DECISIONS.md` | Auto-written run log. Skim only. |
| `/workspace/rudder-v4-restart-handoff.md` | Stale, outside the repo. |

## 2. Where main is (step 9, 2026-11-04)

Collapsed sidebar sections (review, done, closed) expand their agent rows in
the sidebar when clicked or opened with Enter. The worker pane continues to
show the selected agent. Click the header again or press Esc to collapse.

**4.0.0 shipped 2026-11-04.** npm `latest` is 4.0.0 for `@exla/rudder` and
`@viraatdas/rudder` (tag v4.0.0, commit 500e5c3b, all release checks green on
Linux, macOS arm64 and Intel). Cloud control plane deployed 16:25Z running
worker image `registry.fly.io/rudder-workers-exla:4.0.0`; homes adopt it when
idle. The image was built before the last macOS test-only fixes; Linux
behavior differs only in the migration path-spelling fix.

PLAN.md section 0 steps 0-8 are merged. Step 9 results:

- Full suites green: Node 943/943, TUI 59/59 (one load flake passes alone),
  cargo workspace, Cloud build, release build.
- Real sessions (Claude and Codex, local CLI) on a Python repo and a Node repo
  with dependencies. Forks start in about 0.1-0.3s and use about 40K of disk
  against 23M for a full checkout. Agents see current code plus warm
  `node_modules`/build output, run tests inside the fork, and merge on their own.
- Fixed during step 9: CLI workers never integrated; forks used the snapshot's
  stale commit; headless backends ran outside the fork mount; login shells
  inside forks picked stock jj; auto-integration raced the plan scheduler.

Agent context: graphs at the bottom of the sidebar (`context_panel.rs`) for
the selected agent: window fill (cached, cache write, fresh, free), the
estimated fixed base (first request since start or compaction) against the
conversation, context per request (last 60), and the time split. The agent
card shows no context lines. Since 4.2.12, the panel uses at most a third
of the sidebar. 4.3.1 replaces the solid blocks and repeated
legends with a thin context meter, a small growth trace, and text summaries:
3 rows for usage/cache, 5 with growth/time, 7 with base and time detail.
It keeps at least 10 agent-list rows, uses only 2 while
waiting, and hides in narrow layouts or when Alt+h hides the agent panel. The diff panel keeps its text lines. Subagent trees use only a fold glyph
without counts, and drawer headers omit expand/collapse instructions. Codex
reports capacity; Claude only for a `[1m]` model, and its compaction marker is
an observed auto `compact_boundary`. Parsed off the UI thread every ten seconds.

Merge conflicts: a run someone meant to merge no longer waits on a conflict.
That is a planned node nobody has sent a follow-up to, or a run the user
pressed `m` on (recorded as `mergeRequested` in run.json, cleared by a new
prompt). The dashboard merges it again against the latest main change off the
UI thread, one per project at a time (the rest queue), settles mechanical
files, and starts the resolver in its row for what is left. Such rows found in
`merge-conflict` at startup are picked up the same way. Two resolver attempts
(counted from `resolver` turns in run.json), then "needs you" with the reason;
`m` grants one more. Any other conflicted row (a dashboard worker that
conflicted under older code) shows "conflict · press m to resolve" and is never
merged on its own. Automatic
integration no longer rebases or runs checks before merging.

Merge history: a merge lands on the main bookmark, not on the checkout's
working-copy commit. It fast-forwards when the agent started from the current
main and otherwise makes one merge commit with main as its first parent; main
never gains an empty, message-less commit. Afterwards the checkout sits on the
new main with its own edits still uncommitted and Git HEAD attached, so
`git status` reads "On branch main". A conflicted merge is resolved in the
checkout only when it has no edits of its own; otherwise the merge stops and
asks for them to be committed or discarded. Pinned by
`tests/jj-merge-history.test.mjs` (real jj and git).

Selection speed: moving the selection to a busy agent now shows its current
screen on the next frame. The selected pane drains its whole output queue
before drawing (it used to replay up to 256 KB one 64 KB batch per frame), and
input is handled before the poll, so a PTY wake is no longer swallowed (that
cost a 33 ms tick). Key-to-frame on the host was already about 1 ms; on a Cloud
home the rest is the network and the home relay (`home.mjs` blocks its event
loop for about 80 ms every 5 s while it spawns the preview scan).

Linux fork creation: moving a fork from its snapshot to the base used three
jj processes per differing file (182 s for a 9,294-file branch fork of
libra). It is now one Git two-tree checkout (`git read-tree --reset -u`) with
a throwaway index, run through the mounted overlay, with checkout conversions
and repo hooks turned off (needs Git 2.42+ for `--attr-source`, else a full
checkout). Synthetic repo: 4,000 files 55.8 s to 0.6 s; 9,300 files about
1.2 s. A launch now uses the ready snapshot nearest its base (not the newest),
starts a warm build for a branch base too, and waits up to 90 s for that build
when every snapshot is over 500 files away. Conflicted bases get a full checkout.

Workspace preparation no longer freezes the dashboard. A new task, a plan
node, `b` (branch) and `/handoff` add their row at once as "preparing
workspace"; `rudder __launch-node` and the Claude transcript copy run on a
background thread (one at a time, as before), and the App loop starts the
agent when the result arrives. Keys typed into the pane meanwhile reach the
agent once it starts; `x` or `dd` cancels and the workspace is removed when it
finishes; a quit drops the row and leaves any late workspace to the orphan
sweep. `/gam` still prepares its generator's workspace on the loop. On
2026-11-06 a 182s `__launch-node` froze a libra dashboard for its whole run.

Warm snapshots follow trunk (src/warm-snapshot.ts header). One detached
builder per project (started by launches and clean integrations, exits after
30 idle minutes) polls the jj op heads and builds a snapshot whenever
`currentBaseCommit` moves; quick moves coalesce, and after a refresh a small
move waits 60 s (or as long as that refresh took) while a move over 500 files
builds at once. Linux refreshes are a thin OverlayFS layer over the nearest
snapshot's layers: the builder mounts the stack at the fixed path
`.warm/build`, applies the base change with the same Git two-tree checkout and
runs the warm commands there, so outputs are reused at the paths they were
made at. `.warm/<key>.lowerdir` lists the stack; the fork enter script mounts
it and binds the fork at `.warm/build` so paths baked into build output
resolve. At 8 layers the next refresh copies the merged view into a plain
directory. rudder-jj's overlay snapshot reads `.gitignore` through the stack.
macOS refreshes are APFS clones of the previous snapshot (not run on a Mac
yet). Branch launches no longer build: they fork the nearest base snapshot
and patch. A project's first launch waits up to 90 s for its first snapshot.
Pruning keeps the newest snapshot plus every layer a kept snapshot or a fork
(`.fork/lower` or its current lower) mounts. New forks link trees under
`.warm/lower-links`, so forks with the old one-layer enter script never
resolve a tree to a stacked snapshot. Synthetic pnpm project (3,000 files,
Next/React/AWS SDK deps, incremental tsc, 425 MB of node_modules and dist):
a 10-file base move is ready 2.6 s later; the refresh itself takes 1.6-1.9 s
against 1.8-2.1 s from scratch and adds 0.9 MB of disk against 427 MB; a fork
of it takes 0.09 s; compaction took 2.1 s.

After 4.0.0 (2026-11-04): files moved into a fork keep the executable bit;
`rudder merge` (and the dashboard `m`) merges first and then runs the
rudder.toml check once inside the workspace, reporting a failure without
undoing the merge (`--skip-checks` skips it); `rudder ls` no longer shows zero
phase times for headless runs; `LocalStore::gc` refuses stores that hold jj
objects. Fork commits stay in Git on purpose: a fork is a workspace of the
user's colocated repo, so it shares that repo's backend.

macOS agents now get forks too (src/warm-snapshot.ts `createCloneWorkspace`,
`rudder-jj rudder-clone-add`): warm snapshots build locally as on Linux, and
each agent workspace is one APFS `clonefile` of the snapshot that keeps its
recorded jj file states, then checks out the base (only differing files are
written). It is a plain local jj workspace, so nothing runs through `enter`.
On a 5,000-file Node repo a fork takes about 0.14s against 1.25s for a full
checkout, with `node_modules` already there. Another volume or a failed clone
falls back to a full checkout; there is no byte copy. Codex agents launched
from TS now get the tool cache through `shell_environment_policy.set`, since a
user policy like `inherit = "core"` hid it from Codex's shell; native Codex
launches (native/src/launch.rs) pass the same `-c` arguments. A Mac warm build
only starts with 10 GB free; a project opts out with `[warm] commands = []`.

Shipped in 4.2.2/4.2.3 (2026-11-05; the v4.2.1 tag never published: the
macOS clone change broke the Linux-only rudder-jj fork_add, which a Mac
build does not compile). Control plane v187 runs worker image 4.2.3; the
home adopts it on its first idle window.
- Enter on a finished agent's worker pane resumes or reopens it, like Enter on
  its row; ⌥h full screen animates (about 200ms, 30 fps, PTY resized once).
- Predictive echo no longer turns itself off at normal typing speed (keys
  typed during a hold were re-arming it) and recognizes Rudder's own task
  hint. Emulated at production delays, per-character lag at 40-160ms gaps went
  from about 193ms to 2ms (`node scripts/cloud-perf.mjs`).
- Typing in a read-only `rudder cloud` terminal takes control instead of
  dropping the keys (a slept laptop's dead socket kept control for up to 30s).
- Measured but not changed: the control plane machine runs in iad although
  fly.toml says sjc, so every keystroke crosses the US twice (about 180ms of
  the 190ms echo). Moving it west is the largest remaining win; its S3 buckets
  are in us-east-1. Attach also replays up to 256KB that the repaint it asks
  for makes redundant.

Stray `[<0;23;3m`-like text and flicker while typing in `rudder cloud`
(4.2.10, client side, src/key-sequences.ts): predictive echo treated the
bytes of escape-sequence keys (mouse clicks, arrows, Shift+Enter, focus and
paste markers) as typed characters. Since the held-keys change in 4.2.2 it
drew them at the caret on the next keypress, plus the already echoed keys
again, then rolled them back. It now skips whole sequences and pastes, and a
rollback that lands mid-frame repaints at the frame's end instead of leaving
the cells drawn. The client also sends keys in whole escape sequences: a read
that split one (`ESC` | `[<0;23;3M`) reached the dashboard's crossterm as Esc
plus typed text, and a split color reply was typed as `11;rgb:...`. Other
terminal replies (`CSI ?`/`>`/`=`, OSC, DCS, APC) are dropped from keys. The
Home's preview scan queries projects one at a time: each spawn forks the
supervisor (about 5ms at its size), so batches of four held keystrokes and
frames for about 20ms each.

The same stray text, fixed where every client's keys land (4.2.12,
cloud/worker/terminal-transport.mjs `createKeyFramer`): the 4.2.10 fix only
helps a laptop running 4.2.10 or later, and the machine wrote each socket
message to the dashboard's PTY as its own write (node-pty queues one
`fs.write` per call), so a split from an older client reached crossterm as a
lone ESC (Esc) plus the tail typed. The transport now holds a sequence that
ends a message for up to 50ms, writes a lone Esc on its own, drops a sequence
cut short, passes bracketed pastes through untouched and drops terminal
replies, which crossterm types even when whole (OSC as Alt+] plus
`11;rgb:...`, `CSI >` as `1;10;0c`) or takes every key after (`CSI ?...n`
until a "c" or "u"). Home dashboards and sails both go through it. The
browser app sends composed text and named keys over HTTP, and the private
access terminal is read-only, so `rudder cloud` is the only raw key path.
tests/cloud-dashboard-keys.test.mjs splits a burst at every byte against a
model of crossterm 0.29's parser.

`/sync` (native/src/agent_sync.rs): bare `/sync` now tells every running and
review agent in the project, main included, what the others are doing: one
compact message (about 2,000 characters at most) built from run records and
in-memory pane state, typed into each agent only once it is idle at its prompt,
so busy agents get it when their turn ends. It is context, not a follow-up: no
user turn, merge requests and planned-node auto-merge untouched, and the reply
turn ends with no review ping. The cloud machine sync moved to `/sync cloud`
(`/sync all` and `/sync move` unchanged). See engineering-reference section 7.

Subagents in the sidebar (native/src/subagents.rs, not released yet): a
Claude Code or Codex agent that spawns subagents shows `▸ N subagents` on its
card; Tab in the agent list opens a tree with each one's status and time, j/k
walk it, Tab closes it. Tab also opens and closes the done/closed drawers.
Claude reports through `SubagentStart`/`SubagentStop`/`PostToolUse`(Agent)
hooks plus task notifications; Codex through `SubagentStart`/`SubagentStop`,
named from its rollout, and it never reports a failed subagent. View state
only. See engineering-reference section 7.

Codex rows from the rollout (native/src/codex_rollout.rs, not released yet):
a background scan reads each live Codex worker's rollout once a second. A done
row reopens only for a turn the rollout shows running (`task_started` with no
`task_complete`/`turn_aborted`) that is not the one `<signal>.turn` last
reported done, or for the UserPromptSubmit hook; Esc, F2, `/` and Backspace no
longer leave it "working". Questions (`request_user_input`, accepted
`request_user_input_async`) and a finished plan on "Implement this plan?" show
"needs input"; the plan only while the pane's screen shows that menu, saved as
run.json `codexPlanMenu`, which also gates the phone's plan card (Esc on the
menu writes nothing to the rollout). The rules mirror `pendingCodexAsks` in
cloud/worker/asks.mjs; both sides are pinned to `tests/fixtures/codex-0.170-rollout*` (Rust
`codex_rollout::tests`, JS `tests/codex-rollout-fixture.test.mjs`), so change
them together. After `/new`, `/clear` or "clear context and implement" the row
moves to the new thread: the worker's SessionStart hook names it (measured on
0.170: it fires at the new thread's first turn, `source: "clear"` for
`/clear`), and the row adopts it when that rollout began after the pinned one
in the same folder.

Rudder's ignore entries (RUDDER.md, RUDDER_SHARED.md, `.rudder-workspaces/`,
the orchestrator skills) go in `.git/info/exclude`, never `.gitignore`; jj
0.40.0 honors it. jj ignores Git's user.name/email, so with no jj identity the
dashboard and `rudder doctor` say commits would read "JJ_EMPTY_STRING" and
print the `jj config set --user` commands. Commit subjects cut at a word.

Kimi and GLM agents (native/src/openrouter.rs, not released yet): `kimi` and
`glm` are two more rows in the `/model` picker (also `/model kimi`,
`--model glm`). They run Claude Code on OpenRouter (Kimi K3.1 Pro, GLM 5.3)
through that one process's env; everything else is the Claude path. The key
comes from `OPENROUTER_API_KEY` or opencode's login. Cloud homes need
`rudder cloud secrets set OPENROUTER_API_KEY` (opencode's `auth.json` is not
synced). See engineering-reference section 10.

Dashboard look for 4.2 (native render.rs/theme.rs): agents are cards (title,
then a colored status line with elapsed time and +N -N; the selected card is a
rounded outline), a header row (wordmark, project, branch, version), a
one-line prompt pill, a footer with the keys for the current pane and `?` for
all keys. Motion only while something moves (about 11 fps cap): a 600ms
startup reveal any key skips, a shimmer on working cards, a short glow when a
card changes state. Notices show as a pill and then settle into the footer and
never resize the prompt. Pill words and footer keys use the terminal's own
text color (`term_fg()`, `Color::Reset`), so they read on a light, dark or
unreported background; severity shows in the glyph and border. The faint grey
is for hairlines and separators only, never words (theme.rs has a contrast test).

## 3. Status of every agent

Plan lanes (all merged locally):

| Node | Area | Result |
|---|---|---|
| n0 | Single-owner real Fly qualification | INCOMPLETE. Held by user rule (no Fly runs or remote builds without approval). SDK/runtime/ordinary stages never reached on Fly. |
| n1 | Local SDK end to end | Done. 7/7 real scenarios via `scripts/test-sdk-helper-local.sh --real`. |
| n2 | Compiler isolation without Landlock | Done, 11 kernel adversarial tests. |
| n3 | Engine restart recovery, normal drain | Done, 14 recovery + 31 durability tests. |
| n4 | Enforced managed compiler cache | Not qualified. Reuse later disabled fail-closed (harden-4, harden-9). |
| n5 | App owner startup/shutdown | Done, 9 lifecycle tests. |
| n6 | Existing-user migration and rollback | Done. |
| n7 | Real model profiles | Done. |
| n8 | Public command surface (45 ops registry) | Done; E11 caller migration incomplete. |
| n9 | File/document viewer | Done, 27 tests. Real-browser check pending. |
| n10 | Agent efficiency, scoped memory | Code done; E14 has zero real samples. |
| n11 | Automatic planning, durable tasks | Done, 95 tests. |
| n12 | Private previews, responsive terminals | Done, 43 + 15 tests. |
| n13 | Integration wiring | Done; two installed-provider tests skipped. |
| n14 | Launch gates / readiness doc | Done. Its "stable blocked" verdict used the full E0-E14 bar, since replaced by the Cloud-first scope. |

Hardening waves (all merged locally):

| Run | Result |
|---|---|
| harden | Partial. Final owner release blocked on machine-drain proof. |
| harden-2 | Attach socket fence, done. |
| harden-3 | Migration wiring, done. |
| harden-4 | Managed compiler execution disabled fail-closed. |
| harden-5 | Preparation binary tests, done. |
| harden-6 | Production cancellation, done (merge conflict resolved). |
| harden-7 | Ordinary SDK reuse fail-closed. |
| harden-8 | Partial, both findings open. |
| harden-9 | Compiler input hardening; reuse still disabled. |
| harden-10 | Runtime producer census. `other_runtime_producers` still Pending. Final additions were merged UNCOMPILED; verify they build. |
| harden-11 | Root/SDK storage retirement + `owner_reconciliation.rs`. 3 retained-host regression tests failed fixture validation before merge; confirm whether fixed or ignored. Production external-work adapters incomplete. A follow-up "Hardening pass (2 findings)" also merged. |
| harden-12 | `managed_machine_drain.rs` RetainedMachineDrain. Final release only yields Pending/Uncertain; needs harden-10/11 types plugged into its seam. |
| harden-13/14/15 | Cancelled before doing work. |

Dropped queued follow-ups (from harden-11): tests for build admission
closure in `compiler_sdk_client`, `SdkProcess::retire_startup_retaining()`
startup retirement tracking, and reconciliation in
`engine_host_ingress_acquisition.rs`.

Other runs: TestFlight session, morning digest and sign-out merged;
cloud-sync paused; `main-branch-jrqtg` (Sol) completed; `ps` cancelled
(accidental launch); planner `finish-rudder-v4-plan-fucki` deleted.

## 4. Why this went off course

- Hardening waves close partial subsets ("fail-closed", "remains Pending")
  and each spawns more follow-ups. Depth limit 3 per node still allowed 15
  hardening runs. More local hardening cannot reach stable.
- The stable gates that remain need things local work cannot provide:
  E2 (two real Fly workers), E5/E10 (replacement worker restore), Mac
  hardware gates, real-model samples, E14 efficiency samples.
- Every Rust check goes through one shared cargo queue (one big rustc on
  8 cores). About 6 minutes wall time for a test that runs in 0.25s.
- Fly qualification ran ~10 remote builds for 5 revisions over a day
  before the user stopped it.

## 5. Two hard limits shipped on purpose

1. Managed compiler cache reuse and managed Buck2 execution are disabled
   until writer isolation is enforced.
2. Managed runtime producer census is Pending, so final managed owner
   retirement cannot certify release.

## 6. Decisions the user needs to make

- DECIDED 2026-11-04: the plan is PLAN.md section 0. 4.0.0 ships after
  step 9. The user approves the release and Cloud deploy.
- Whether to approve the one optional Fly campaign (single retained owner,
  SDK + runtime + ordinary on one image) described in the readiness doc.
- Cloud deploy (worker image + Fly control plane) needs explicit approval.
- `origin/rudder/mac-pty-diagnostic`: 7 unmerged PTY/broker commits from
  10-30, about 210 behind main. Needs a Mac to verify; merge or drop.

## 7. What gets built

The plan is `PLAN.md` section 0 (steps 0-9). One test gate only, at step 9.
No evidence files, no automatic follow-ups, no prereleases or Cloud deploys
before step 9.

4.1 Cloud capacity is merged onto main (2026-11-04) from
`cloud-capacity-build-pool`; that branch can be deleted. Node 1025/1025 and
the rudder-native cargo suite pass. Not yet released or deployed; see
PLAN.md section 0 "After 4.0.0" for what remains.

Linear (removed 2026-11-18): Rudder no longer has its own Linear
integration (connect, issue mapping, Generate tasks, the webhook, tracking and
the "From Linear" chip are gone). People file issues with the Linear tool in
their agent. Old `rudder_linear_*` tables stay in existing databases, unused.
Still to do outside the repo: delete the Linear OAuth app's webhook, revoke
its workspace authorization in Linear, and unset the `RUDDER_LINEAR_*` and
`RUDDER_TASKGEN_MODEL` Fly secrets.

4.2 external home host (2026-11-05): `rudder cloud host <ssh-target>` runs
the home on any Linux box with Docker (engineering reference §9.6). The
user's home moved off Fly to an AWS m8azn.6xlarge (us-west-2, 5GHz, 24 vCPU,
92GB, 500GB gp3) as a stopgap until a bare-metal box is ready. The disk was
copied under the 4.1.2 image with the Fly machine stopped (`init.exec` set to
sleep); on 2026-11-05 after the 4.2.0 deploy, `rudder cloud host` switched it
to external mode on `rudder-workers-exla:4.2.0`. The old Fly machine
(8edd225a7d0378) and its 150GB volume were destroyed on 2026-11-05 at the
user's request; Cloud never calls Fly for an external home. Agent scratch now lives
in `/workspace/scratch/<run>` and is cleaned hourly. Release worker images:
`node scripts/worker-image.mjs <version>` after npm publish (about 2 minutes,
no Rust compile; see cloud/README.md).

Fly cleanup and builds (2026-11-05): the failed home-223b (still running on
Fly), 26 stale v2/v3 workspaces, 14 old sails and 24 orphan volumes were
deleted through the control plane's delete reaper and `flyctl`; the empty
`rudder-v4-qualification` app is gone; the karna-montage v3 workspace is
stopped. Left on Fly: the control plane, three stopped
homes of other accounts (two people and the App Store review account) and the
stopped dad-job-apply workspace. The burst pool stays off (an external host
cannot reach Fly's private network); `rudder-burst` now runs the command on
the home instead of failing. 4.2.4 ships sccache in the worker image with
per-workspace keys; 4.2.5 keeps the tool cache (and CARGO_HOME) where it was,
since a moved CARGO_HOME rebuilds every dependency. Cloud runs the 4.2.5 image
(2026-11-06); these reach the home when it next updates while idle. Warm snapshots were rebuilt by hand for the
jj projects with something to build (exla-ai-rudder, libra, offstage,
vibeprint; viraat-gazette needs an uncommitted data file to build); an earlier Rudder one failed inside an old sccache
binary. Repos without root lockfiles (smol, snap, raindrop, ...) get a plain
checkout, and non-jj repos get a snapshot after their first worktree agent.

Update now (2026-11-06, 4.2.6): when Cloud runs a newer worker image than
the home, its dashboards show "update" in the footer, and Alt+r twice updates
the home at once, busy or not (engineering reference "Update now"). Agents
that were mid-turn reopen with one "continue where you left off" turn. The
worker image also carries headless Chromium's libraries now.

Stuck home updates (2026-11-06, for 4.2.9, not released): home-53c2 stayed
on 4.2.0 because its host's `docker login` expired, the pull failed, and the
pending replacement token then refused every Alt+r and idle update forever.
Now a pending token stops blocking after 20 min (control plane only, so the
4.2.0 home unblocks once Cloud deploys), the host writes why it failed next
to `desired.failed.json`, and 4.2.9 homes report it so Cloud clears the token
at once and the footer shows the reason (engineering reference 9.6). Still
on the user: log home-53c2's host in to registry.fly.io again, then press
Alt+r; rerun `rudder-host-setup` there to get the new apply script. Open
decision: the hand-made, expiring registry login. Recommended: also push each
worker image to a public registry (ghcr.io; the image holds no secrets and the
repo is public) and point external hosts at it, so hosts need no login at all.

Public worker image (2026-11-06, for 4.2.9, not released): CI now pushes every
release's worker image to the public `ghcr.io/exla-ai/rudder-worker:<version>`
(`.github/workflows/worker-image.yml`, on `v*` tags after npm serves the
version, or by hand with `-f version=X`). New external hosts run it with no
login. Cloud sends an external home the new tag in the repository it already
runs, so GHCR hosts update with Alt+r and old registry.fly.io hosts behave as
before. To move home-53c2 off the expiring Fly login (on its host, once 4.2.8
is public there): `sudo docker pull ghcr.io/exla-ai/rudder-worker:4.2.8`, then
`echo ghcr.io/exla-ai/rudder-worker:4.2.8 | sudo tee /etc/rudder/image` and
`sudo rudder-home-start`.

Private worker images for external hosts (2026-11-06, after 4.2.9): the GHCR
package could not be made public, so external hosts pull from
`registry.fly.io/rudder-worker-images`, a Fly app with no machines and no
secrets. `scripts/worker-image.mjs` copies each release there, and Cloud sends
hosts on it updates there. A deploy token for that app alone (5 years) is the
host's one login. 4.2.9 is copied (same digest). For home-53c2 the token was
left at `/workspace/.rudder-host-pull-token` (host path
`/srv/workspace/.rudder-host-pull-token`) for the user's one-time host step,
which deletes it.

Fewer checks (2026-11-06, not released): the user wants agents to test and
use CI sparingly. Worker prompts now say "run one quick check for what you
changed; add a test only for a real bug or tricky logic", the default
`Done when:` is "the task is implemented and works as asked", and planners
name at most one targeted check in `success`. AGENTS.md keeps the full suite
for releases only.

Cloud link clicks (2026-11-05, not released): a click on a link in a
`rudder cloud` pane ran `xdg-open` on the home, which has none. The dashboard
now asks the attached laptop to open it (engineering reference "Clickable
links"). Takes effect once the home runs the new dashboard (worker image) and
the laptop runs a `rudder` release with `src/link-open.ts`.

Alt+h (4.2.12): hides or shows only the left agent panel and
bottom task panel. The worker pane stays visible and expands into their space;
the header, footer and open diff stay visible. Toggling back restores focus
and the task draft. Focusing a hidden panel shows it again.

## 8. Disk

Cloud build speed (2026-11-06): inspected the running external home: 24 CPUs,
92 GiB RAM, no visible CPU quota, and ample idle capacity. The scheduler's
parallel limit is already 1000; it was not throttling agents to the old home.
The local `/workspace/rudder-build/rcargo` still forced six build jobs and both
build wrappers checked an absent sccache path and used an 800 MB cache budget.
Both wrappers now use `cloud/host/build-resources.mjs` to size builds from CPU,
memory and cgroup limits (22 jobs here), preserving explicit job overrides.
They discover sccache on PATH and use the agent tool cache with a 20 GiB default.
The image-pinned sccache 0.18.0 was missing from the running container; installed
its checksum-verified binary in `/home/rudder/.cargo/bin` without restarting
agents. A real Rust compile probe produced a miss followed by a hit. The shared
Cargo target lock remains intentional. These wrapper edits are local host setup,
not tracked product files; keep the helper available at the current checkout path.
Native and headless prompt guidance now asks agents to inspect current host
limits and wait on commands directly while reporting progress. Existing sessions
retain their prompts; updated prompt guidance needs a later build/install.
The observed 50-second sleep loop was registry-release polling, not a scheduler
delay. No model preferences changed. These changes are included in the combined
4.2.8 release candidate; publication and Cloud adoption must be verified separately.

Cleaned 2026-11-04: Fly qualification captures, old scratch dirs and stale jj
workspaces are gone (`/workspace` 85%, 22G free). Old qualification `*.py`
scripts moved to `/workspace/rudder-scratch/old-py/`. The shared cargo target
`/workspace/rudder-build/target` (16G) is kept as the build cache.

Antigravity models: the worker pins the current CLI release and builds its model
picker from `agy models`, grouping Google thinking-level variants while keeping
the IDs the CLI accepts. Google only lists models available to the signed-in
account; Rudder does not invent plan-only models.
