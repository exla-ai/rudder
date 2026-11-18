// Local development server for the Rudder phone app. No dependencies.
//   PORT=4321 node cloud/app/dev-mock.mjs
// Serves /app/* from this folder and mocks /api/app/*.
// Mock helpers: /app/__mock/signout (simulate signed out), /app/login (sign back in),
// /app/__mock/home/<running|stopped|failed|none> (machine states),
// /app/__mock/creds/<out|claude|both|expired> (Claude Code / Codex sign-in),
// /app/__mock/usage/<ok|old> (plan usage, or the error of a home too old to report it),
// /app/__mock/fresh (a brand new account: no GitHub, no agent login, no repos),
// /app/__mock/computer (prints a code to approve, as `rudder login` would),
// /app/__mock/onboard (show the setup picker again), /app/__mock/integrations/<on|off|unset>,
// /app/__mock/invite (prints a join link), /app/__mock/files/agent-edit (an agent rewrites README.md
// in run_a1's workspace, to see the Files panel's conflict banner; build cloud first for its renderer). Orgs, invites, Slack and a plan are mocked too,
// and so is the team view (two teammates in Exla, one asleep; uses cloud/dist/team.js).
// The digest uses the real worker/digest.mjs and cloud/dist/digest.js (build cloud first).
import http from "node:http";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { createHash } from "node:crypto";
import { digestRun } from "../worker/digest.mjs";

const DIR = path.dirname(fileURLToPath(import.meta.url));
const PORT = Number(process.env.PORT || 4321);
const MIN = 60_000;
const now = () => Date.now();
const iso = (ms) => new Date(ms).toISOString();

let signedOut = process.env.MOCK_SIGNED_OUT === "1";
let homeMode = "running";
let usageMode = "ok";
// Who the machine's Claude Code and Codex sign in as, and sign-ins in flight.
const signedIn = (provider) => ({ ok: true, account: "viraat@example.com", plan: provider === "claude" ? "max" : "pro", detail: null });
const CREDS = {
  out: () => ({ claude: { ok: false, account: null, detail: "not logged in" }, codex: { ok: false, account: null, detail: "not logged in" } }),
  claude: () => ({ claude: signedIn("claude"), codex: { ok: false, account: null, detail: "not logged in" } }),
  both: () => ({ claude: signedIn("claude"), codex: signedIn("codex") }),
  expired: () => ({ claude: { ok: false, account: "viraat@example.com", plan: "max", detail: "login expired" }, codex: signedIn("codex") }),
};
let creds = CREDS.claude();
const logins = {};
// GitHub in the vault, and the device flow the phone runs (approves itself after a few polls).
let githubConnected = true;
let githubFlow = null;
// Computers waiting on `rudder login`: code -> { deviceName, expiresAt }.
const computers = new Map();
let freshProjects = null;
// The "All agents" chat's hidden project: null until the app opens it.
let coordinator = null;
const T0 = now();

const home = () => homeMode === "none" ? null : ({
  id: "ws_home_7f3a",
  status: homeMode,
  machineState: homeMode === "running" ? "started" : homeMode === "stopped" ? "suspended" : homeMode === "queued" ? "starting" : "failed",
  connected: homeMode === "running",
  failureReason: homeMode === "failed" ? "Fly machine failed health checks after boot (exit 137, out of memory)." : null,
  activeAgents: projects.reduce((n, p) => n + p.agents.filter((a) => a.status === "working").length, 0),
  lastActivityAt: iso(now() - 2 * MIN),
  tailnet: { hostname: "rudder-home", dnsName: "rudder-home.tail4c2e1.ts.net", online: homeMode === "running", https: true },
  credentials: homeMode === "running" ? { ...creds, github: githubConnected ? { ok: true, account: "viraatdas", detail: null } : { ok: false, account: null, detail: "no GitHub token in the vault" }, checkedAt: iso(now()) } : null,
  logins,
});

const agent = (o) => ({
  phase: null, model: null, mode: "execute", lastMessage: null, summary: null, nodeId: null, planId: null, remoteUrl: null, visibility: "team",
  createdAt: T0 - 90 * MIN, updatedAt: T0 - 10 * MIN, ...o,
});

const projects = [
  {
    workspaceId: "wsp_rudder", name: "rudder", repo: "exla-ai/rudder", ref: "main", status: "running", failureReason: null,
    dashboard: "running", clients: 1, lastActivityAt: iso(T0 - 1 * MIN),
    agents: [
      agent({ id: "run_a1", title: "Phone app for agent control", task: "Build the phone-first web app under cloud/app", status: "working", backend: "claude", model: "opus", mode: "main", updatedAt: T0 - 1 * MIN, lastMessage: "Wiring the transcript poll to keep scroll pinned only when you are already at the bottom.", remoteUrl: "https://claude.ai/code/session_01Abc" }),
      agent({ id: "run_a2", title: "Fix flaky Ctrl+W chord test", task: "Stabilize the e2e chord tests", status: "waiting", backend: "codex", model: "gpt-6.1-sol", phase: "permission", updatedAt: T0 - 4 * MIN, lastMessage: "I need permission to run `cargo test --release` with network access. Allow?" }),
      agent({ id: "run_a3", title: "Tailnet HTTPS for previews", task: "Expose dev servers over tailnet https", status: "review", visibility: "private", backend: "claude", model: "sonnet", nodeId: "n2", planId: "plan_1", updatedAt: T0 - 18 * MIN, lastMessage: "Done. Ports 3000 and 5173 are reachable at https://rudder-home.tail4c2e1.ts.net. Ready to merge." }),
      agent({ id: "run_a4", title: "Delete cloud workspaces safely", task: "Request + reaper for deletes", status: "done", backend: "claude", model: "opus", updatedAt: T0 - 3 * 60 * MIN, lastMessage: "Added the deletion reaper with a 30s sweep and 410s for dead rows." }),
      agent({ id: "run_a5", title: "Bump jj to 0.40 flags", task: "jj rebase -o", status: "merged", backend: "codex", model: "gpt-6.1-sol", updatedAt: T0 - 26 * 60 * MIN, lastMessage: "Merged into main." }),
      agent({ id: "run_o1", title: "Plan: preview links everywhere", task: "Make every dev server reachable from the phone", status: "waiting", backend: "claude", model: "opus", mode: "rudder-plan", planId: "plan_1", updatedAt: T0 - 3 * MIN, lastMessage: "Here is the plan: 5 tasks. Approve it to launch, or tell me what to change." }),
      agent({ id: "run_a7", title: "Port scanner for the home", task: "List listening ports per project", status: "working", backend: "codex", model: "gpt-6.1-sol", nodeId: "n1", planId: "plan_1", updatedAt: T0 - 2 * MIN, lastMessage: "Reading /proc/net/tcp6 for owner pids." }),
      agent({ id: "run_a6", title: "Explore opencode resume", task: "opencode session resume", status: "stopped", backend: "opencode", model: "anthropic/claude-sonnet", updatedAt: T0 - 50 * 60 * MIN, lastMessage: "Stopped by you." }),
    ],
    plan: {
      updatedAt: iso(T0 - 3 * MIN),
      awaitingApproval: true,
      plans: [{ id: "plan_1", awaitingApproval: true, request: "Make every dev server on the home reachable from the phone" }],
      nodes: [
        { id: "n0", title: "Tailscale in the worker image", status: "merged", runId: "run_a5", backend: "codex", model: "gpt-6.1-sol", deps: [] },
        { id: "n1", title: "Port scanner for the home", status: "running", runId: "run_a7", backend: "codex", model: "gpt-6.1-sol", deps: [{ on: "n0", type: "hard" }] },
        { id: "n2", title: "Tailnet HTTPS for previews", status: "review", runId: "run_a3", backend: "claude", model: "sonnet", deps: [{ on: "n0", type: "hard" }] },
        { id: "n3", title: "Preview links in the phone app", status: "planned", runId: null, backend: "claude", model: "opus", deps: [{ on: "n1", type: "hard" }, { on: "n2", type: "hard" }] },
        { id: "n4", title: "Docs for preview links", status: "ready", runId: null, backend: "claude", model: "sonnet", deps: [{ on: "n2", type: "soft" }] },
        { id: "n5", title: "Plain http fallback", status: "failed", runId: "run_a4", backend: "claude", model: "opus", deps: [{ on: "n2", type: "hard" }] },
      ],
    },
    ports: [
      { port: 3000, url: "https://rudder-home.tail4c2e1.ts.net:3000", label: "board" },
      { port: 5173, url: "https://rudder-home.tail4c2e1.ts.net:5173", label: "vite dev" },
    ],
  },
  {
    workspaceId: "wsp_wager", name: "wagerpals", repo: "viraatdas/wagerpals", ref: null, status: "running", failureReason: null,
    dashboard: "running", clients: 0, lastActivityAt: iso(T0 - 12 * MIN),
    agents: [
      agent({ id: "run_b1", title: "Settle bets on resolution", task: "Auto-settle when a market resolves", status: "working", backend: "claude", model: "opus", updatedAt: T0 - 2 * MIN, lastMessage: "Running the settlement test suite (42 of 118 passing so far)." }),
      agent({ id: "run_b2", title: "Stripe payout webhook", task: "Handle payout.failed", status: "failed", backend: "codex", model: "gpt-6.1-sol", updatedAt: T0 - 35 * MIN, lastMessage: "Error: STRIPE_WEBHOOK_SECRET is not set in this environment." }),
      agent({ id: "run_b3", title: "Leaderboard query perf", task: "Speed up leaderboard", status: "done", backend: "claude", model: "sonnet", updatedAt: T0 - 5 * 60 * MIN, lastMessage: "Leaderboard p95 went from 840ms to 62ms with a covering index." }),
    ],
    ports: [{ port: 3000, url: "https://rudder-home.tail4c2e1.ts.net:3001", label: "next dev" }],
  },
  {
    workspaceId: "wsp_site", name: "rudder-site", repo: "viraatdas/rudder-site", ref: "launch", status: "running", failureReason: null,
    dashboard: "cloning", clients: 0, lastActivityAt: iso(T0 - 3 * 24 * 60 * MIN),
    agents: [],
    ports: [],
  },
];
// No project has been moved yet (POST /api/app/project-order).
for (const p of projects) p.sortOrder = null;


// Morning digests: run records as the dashboard writes them, through the real
// worker reader and the control plane's composer.
const digestLib = await import("../dist/digest.js").catch(() => null);
const HOUR = 60 * MIN;
const digestRecords = (end) => ({
  wsp_rudder: [
    { id: "run_a1", taskSummary: "Phone app for agent control", task: "Build the phone-first web app under cloud/app", status: "running", backend: "claude", model: "opus", createdAt: end - 14 * HOUR, updatedAt: end - 20 * MIN,
      turns: [{ prompt: "Build the phone-first web app", ts: end - 14 * HOUR }, { prompt: "make the transcript sticky at the bottom", ts: end - 9 * HOUR }, { prompt: "also show tool calls collapsed", ts: end - 3 * HOUR }] },
    { id: "run_a3", taskSummary: "Tailnet HTTPS for previews", task: "Expose dev servers over tailnet https", status: "completed", lifecyclePhase: "review", backend: "claude", model: "sonnet", nodeId: "n2", createdAt: end - 11 * HOUR, updatedAt: end - 7 * HOUR,
      doneSummary: "Listening ports on the home machine get `tailscale serve --https`; falls back to plain http when tailnet certs are off.", publish: { url: "https://github.com/exla-ai/rudder/pull/212", number: 212 }, turns: [] },
    { id: "run_a4", taskSummary: "Delete cloud workspaces safely", task: "Request + reaper for deletes", status: "completed", lifecyclePhase: "deployed", backend: "claude", model: "opus", createdAt: end - 20 * HOUR, updatedAt: end - 12 * HOUR,
      doneSummary: "Added the deletion reaper with a 30s sweep; dead rows answer 410 to heartbeats.", delivery: { status: "deployed", target: "rudder-cloud-control (Fly)", verifiedAt: iso(end - 12 * HOUR + 5 * MIN) }, turns: [] },
    { id: "run_a5", taskSummary: "Bump jj to 0.40 flags", task: "jj rebase -o", status: "merged", backend: "codex", model: "gpt-6.1-sol", createdAt: end - 6 * HOUR, updatedAt: end - 5 * HOUR,
      doneSummary: "Switched rebase to -o/--onto with a fallback for older jj.", merge: { status: "merged", pushed: true, pushedAt: end - 5 * HOUR + 2 * MIN, targetBranch: "main" }, turns: [] },
  ],
  wsp_wager: [
    { id: "run_b2", taskSummary: "Stripe payout webhook", task: "Handle payout.failed", status: "failed", backend: "codex", model: "gpt-6.1-sol", createdAt: end - 4 * HOUR, updatedAt: end - 3.5 * HOUR, turns: [] },
    { id: "run_b3", taskSummary: "Leaderboard query perf", task: "Speed up leaderboard", status: "completed", backend: "claude", model: "sonnet", createdAt: end - 10 * HOUR, updatedAt: end - 8 * HOUR,
      doneSummary: "Leaderboard p95 went from 840ms to 62ms with a covering index on (season, points).", turns: [] },
    { id: "run_b4", taskSummary: "Ask before placing a bet", task: "Confirm step for bets", status: "running", attention: "permission", backend: "claude", model: "opus", createdAt: end - 50 * MIN, updatedAt: end - 30 * MIN, turns: [] },
  ],
});
function mockDigest(day, end, live = false) {
  if (!digestLib) return null;
  const since = end - 24 * HOUR;
  const records = digestRecords(end);
  const raw = projects.filter((p) => records[p.workspaceId]).map((p) => ({
    workspaceId: p.workspaceId, name: p.name, repo: p.repo,
    runs: records[p.workspaceId].map((r) => digestRun(r, { since, until: end })).filter(Boolean),
  }));
  return digestLib.composeDigest(raw, { day, timeZone: "America/Los_Angeles", since, until: end, generatedAt: end });
}
const digestDay = (ms) => new Date(ms - 7 * HOUR).toISOString().slice(0, 10);
const morning = (() => { const d = new Date(); d.setHours(9, 1, 0, 0); return d.getTime() > now() ? d.getTime() - 24 * HOUR : d.getTime(); })();
const digestStore = [0, 1, 2].map((k) => {
  const end = morning - k * 24 * HOUR;
  return { id: `dg_${k}`, readAt: k ? iso(end + HOUR) : null, createdAt: iso(end), body: mockDigest(digestDay(end), end) };
}).filter((d) => d.body);
const digestPreviewOf = (d) => d && ({ id: d.id, readAt: d.readAt, createdAt: d.createdAt, since: d.body.since, until: d.body.until, ...digestLib.digestPreview(d.body) });

const LONG_OUTPUT = Array.from({ length: 140 }, (_, i) => `test app_tests::case_${String(i).padStart(3, "0")}_${["scroll", "merge", "leader", "render", "signals"][i % 5]} ... ok`).join("\n") + "\n\ntest result: ok. 528 passed; 0 failed; 1 ignored; finished in 41.82s";

function baseTranscript(a) {
  const t = a.createdAt;
  let k = 0;
  const id = () => `m${++k}`;
  const msgs = [
    { id: id(), role: "user", ts: t, text: a.task + ". Keep it phone-first and make transcripts render properly: markdown, code, tool calls." },
    { id: id(), role: "assistant", ts: t + 20_000, text: "I'll start by reading how the control plane exposes workspaces, then sketch the screens.\n\n**Plan**\n1. Read `cloud/src/server.ts` for the existing routes\n2. Define the `/api/app/*` contract\n3. Build the shell, then the three screens\n4. Verify on a 393px viewport in light and dark" },
    { id: id(), role: "tool", ts: t + 30_000, text: "", tool: { name: "Read", input: "cloud/src/server.ts", output: "   1  import http from \"node:http\";\n   2  import { betterAuth } from \"better-auth\";\n   3  import Database from \"better-sqlite3\";\n   4  \n   5  const PORT = Number(process.env.PORT || 8080);\n   6  // ... 2,431 more lines", isError: false } },
    { id: id(), role: "tool", ts: t + 34_000, text: "", tool: { name: "Grep", input: "\"/api/rudder/workspace\" in cloud/src", output: "cloud/src/server.ts:1180:  if (url.pathname === \"/api/rudder/workspace\" && req.method === \"POST\") {\ncloud/src/server.ts:1244:  if (url.pathname.startsWith(\"/api/rudder/workspace/\")) {", isError: false } },
    { id: id(), role: "assistant", ts: t + 60_000, text: "The workspace routes are keyed by `account_id`, so the app API can reuse the same session lookup. Here is the shape I'm going with:\n\n| Route | Method | Returns |\n|---|---|---|\n| `/api/app/state` | GET | home machine, projects, agents, ports |\n| `/api/app/transcript` | GET | messages for one run |\n| `/api/app/send` | POST | `{ ok: true }` |\n| `/api/app/action` | POST | stop or merge a run |\n\nThe keyed list reconcile keeps scroll and expanded tool rows stable across polls:\n\n```js\nexport function reconcile(container, items, keyOf, create, update) {\n  const existing = new Map();\n  for (const child of container.children) existing.set(child.dataset.key, child);\n  let cursor = container.firstElementChild;\n  for (const item of items) {\n    const key = String(keyOf(item));\n    let el = existing.get(key) ?? create(item);\n    update(el, item);\n    if (el !== cursor) container.insertBefore(el, cursor);\n    else cursor = cursor.nextElementSibling;\n  }\n}\n```\n\n> Note: tool output is collapsed by default and truncated past a few KB, so a 10k-line test log never stalls the phone." },
    { id: id(), role: "tool", ts: t + 90_000, text: "", tool: { name: "Bash", input: "npm run check", output: "> @exla/rudder@2.15.4 check\n> tsc --noEmit\n\nsrc/cloud.ts(418,7): error TS2322: Type 'string | undefined' is not assignable to type 'string'.\n  Type 'undefined' is not assignable to type 'string'.", isError: true } },
    { id: id(), role: "assistant", ts: t + 100_000, text: "Typecheck caught a missing fallback in `cloud.ts`. Fixing it:" },
    { id: id(), role: "tool", ts: t + 110_000, text: "", tool: { name: "Edit", input: "src/cloud.ts  (+1 -1)", output: "-  const url: string = state.cloudUrl;\n+  const url: string = state.cloudUrl ?? DEFAULT_CLOUD_URL;", isError: false } },
    { id: id(), role: "tool", ts: t + 150_000, text: "", tool: { name: "Bash", input: "cargo test --manifest-path native/Cargo.toml", output: LONG_OUTPUT, isError: false } },
    { id: id(), role: "system", ts: t + 160_000, text: "Context compacted. Earlier tool output was summarized." },
    { id: id(), role: "user", ts: t + 30 * MIN, text: "Looks good. Can you make the composer stay above the keyboard on iPhone?" },
    { id: id(), role: "assistant", ts: t + 31 * MIN, text: "Yes. The shell now sizes itself to `visualViewport.height`, so the composer rides above the keyboard instead of being covered:\n\n```css\n.shell {\n  position: fixed;\n  top: var(--vvtop);\n  height: var(--vvh);\n}\n```\n\n```swift\n// The native shell injects the token before the page runs.\nlet script = WKUserScript(\n    source: \"window.RUDDER_NATIVE = { token: \\\"\\(token)\\\", platform: \\\"ios\\\" }\",\n    injectionTime: .atDocumentStart,\n    forMainFrameOnly: true\n)\n```\n\nI also check `window.innerHeight - visualViewport.height` to drop the bottom safe-area padding while the keyboard is open. See the [visualViewport docs](https://developer.mozilla.org/en-US/docs/Web/API/VisualViewport) for the event model.\n\n- [x] Composer above keyboard\n- [x] Cmd+Enter sends\n- [ ] Haptics on send (needs the native shell)" },
    { id: id(), role: "tool", ts: t + 33 * MIN, text: "", tool: { name: "WebFetch", input: "https://rudder-home.tail4c2e1.ts.net:3000/health", output: null, isError: false } },
  ];
  if (a.status === "waiting") msgs.push({ id: id(), role: "assistant", ts: a.updatedAt, text: a.lastMessage });
  else if (a.lastMessage) msgs.push({ id: id(), role: "assistant", ts: a.updatedAt, text: a.lastMessage });
  return msgs;
}

// Subagents (worker/transcripts.mjs `subagent`, cloud/app/subagents.js): a
// Claude agent (run_b1) and a Codex agent (run_a7) with subagents done, failed
// and running. A running one gains a step every few seconds, as a live one would.
const subTool = (id, ts, name, input, output, isError = false) => ({ id, role: "tool", ts, text: "", tool: { name, input, output, isError } });
const subSay = (id, ts, text, role = "assistant") => ({ id, role, ts, text });
function liveSubagent(fixed, steps, { every = 5_000, start }) {
  const shown = Math.min(steps.length, 2 + Math.floor((now() - T0) / every));
  const messages = steps.slice(0, shown);
  const done = shown === steps.length && fixed.finishes;
  return {
    ...fixed,
    status: done ? "done" : "running",
    startedAt: start,
    endedAt: done ? messages[messages.length - 1].ts : null,
    toolCalls: messages.filter((m) => m.role === "tool").length,
    result: done ? messages[messages.length - 1].text : null,
    messages,
    earlier: 0,
  };
}
function subagentTranscript(a) {
  const t = now() - 6 * MIN;
  if (a.backend === "codex") {
    const procSteps = [
      subTool("p1", t + 12_000, "exec_command", "sed -n 1,40p /proc/net/tcp6", "  sl  local_address                         rem_address                         st tx_queue rx_queue\n   0: 00000000000000000000000000000000:0BB8 00000000000000000000000000000000:0000 0A 00000000:00000000"),
      subTool("p2", t + 20_000, "exec_command", "rg -n \"inode\" src/ports.rs", "src/ports.rs:41:    let inode = fields[9].parse::<u64>()?;"),
      subSay("p3", t + 31_000, "Listening sockets are state `0A`. The owning pid comes from matching the socket inode against `/proc/<pid>/fd/*` links."),
      subTool("p4", t + 40_000, "exec_command", "cargo test -p ports listen", "running 3 tests\ntest listen_v4 ... ok\ntest listen_v6 ... ok\ntest owner_pid ... ok\n\ntest result: ok. 3 passed"),
      subTool("p5", t + 47_000, "apply_patch", "src/ports.rs", "Success. Updated src/ports.rs"),
      subSay("p6", t + 58_000, "**Parsing works for IPv4 and IPv6.** State `0A` is LISTEN; the inode maps to a pid through `/proc/<pid>/fd`. Three tests pass, including the owner lookup."),
    ];
    const serveSteps = [
      subTool("s1", t + 15_000, "exec_command", "tailscale --socket /tmp/tailscaled.sock serve status --json", "{\n  \"TCP\": { \"3000\": { \"HTTPS\": true } },\n  \"Web\": { \"rudder-home.tail4c2e1.ts.net:3000\": {} }\n}"),
      subSay("s2", t + 26_000, "Serve config is keyed by port, so each scanned port maps to one HTTPS entry."),
      subTool("s3", t + 35_000, "exec_command", "rg -n \"serve\" cloud/worker/tailscale-previews.mjs", "cloud/worker/tailscale-previews.mjs:88:  await runTailnetCommand([\"serve\", \"--bg\", `--https=${port}`, String(port)]);"),
      subTool("s4", t + 50_000, "exec_command", "node --test tests/tailscale-previews.test.mjs", "✔ maps a listening port to its serve URL (4.1ms)\n✔ drops a port that stopped listening (1.9ms)"),
      subTool("s5", t + 61_000, "apply_patch", "cloud/worker/tailscale-previews.mjs", "Success. Updated cloud/worker/tailscale-previews.mjs"),
      subTool("s6", t + 70_000, "exec_command", "node --test tests/tailscale-previews.test.mjs", "✔ maps a listening port to its serve URL (3.8ms)\n✔ drops a port that stopped listening (2.0ms)\n✔ keeps HTTPS for a port that comes back (2.4ms)"),
    ];
    return [
      subSay("x1", t, a.task + ". Use subagents for the two halves.", "user"),
      subSay("x2", t + 5_000, "I'll have one subagent parse `/proc/net` and another map ports to `tailscale serve`, then join the two."),
      { ...subTool("call_proc", t + 8_000, "spawn_agent", "proc_net_parsing", "{\n  \"task_name\": \"/root/proc_net_parsing\"\n}"),
        subagent: { id: "01a1ee20-0000-7000-8000-000000000001", backend: "codex", kind: null, name: "Turing", description: "proc net parsing", prompt: null, status: "done", startedAt: t + 8_000, endedAt: t + 58_000, toolCalls: 4, result: procSteps[5].text, messages: procSteps, earlier: 0 } },
      { ...subTool("call_serve", t + 9_000, "spawn_agent", "serve_mapping", "{\n  \"task_name\": \"/root/serve_mapping\"\n}"),
        get subagent() { return liveSubagent({ id: "01a1ee20-0000-7000-8000-000000000002", backend: "codex", kind: null, name: "Hopper", description: "serve mapping", prompt: null }, serveSteps, { start: t + 9_000, every: 6_000 }); } },
      { ...subTool("call_docker", t + 10_000, "spawn_agent", "docker_ports", "agent limit reached: at most 2 subagents can run at once", true),
        subagent: { id: null, backend: "codex", kind: null, name: null, description: "docker ports", prompt: null, status: "failed", startedAt: t + 10_000, endedAt: null, toolCalls: 0, result: "agent limit reached: at most 2 subagents can run at once", messages: [], earlier: 0 } },
      subTool("x6", t + 62_000, "wait", "/root/proc_net_parsing", "{\n  \"status\": \"completed\"\n}"),
      subSay("x7", t + 70_000, "The parser is done. Waiting on the serve mapping before I wire them together."),
    ];
  }
  const mapSteps = [
    subTool("m1", t + 14_000, "Grep", "resolveMarket  in src", "src/markets/resolve.ts:12:export async function resolveMarket(id: string, outcome: Outcome) {\nsrc/jobs/poll-oracles.ts:57:      await resolveMarket(market.id, result.outcome);"),
    subTool("m2", t + 18_000, "Read", "src/markets/resolve.ts", "  12  export async function resolveMarket(id: string, outcome: Outcome) {\n  13    const market = await db.market.update({ where: { id }, data: { status: \"resolved\", outcome } });\n  14    await emit(\"market.resolved\", market);\n  15  }"),
    subTool("m3", t + 22_000, "Grep", "\"market.resolved\"  in src", "src/markets/resolve.ts:14:    await emit(\"market.resolved\", market);"),
    subSay("m4", t + 30_000, "Nothing listens for `market.resolved` yet, so bets stay open after a market resolves."),
    subTool("m5", t + 34_000, "Read", "src/bets/model.ts", "   1  export type Bet = { id: string; marketId: string; side: Side; stakeCents: number; status: \"open\" | \"won\" | \"lost\" | \"void\" };"),
    subSay("m6", t + 41_000, "**Resolution never reaches bets.** `resolveMarket` (src/markets/resolve.ts:12) marks the market resolved and emits `market.resolved`, but nothing subscribes to it.\n\n- Oracles call it from `src/jobs/poll-oracles.ts:57`\n- A bet's `status` is `open | won | lost | void`; settlement only needs to flip it and credit `stakeCents` times the odds\n\nThe natural hook is a `market.resolved` subscriber."),
  ];
  const payoutSteps = [
    subTool("r1", t + 16_000, "Read", "src/bets/payout.ts", "  8  export const payout = (stakeCents: number, odds: number) => stakeCents * odds;"),
    subSay("r2", t + 24_000, "`payout` returns fractional cents for decimal odds. Checking how Stripe expects amounts before choosing a rounding rule."),
    { ...subTool("toolu_fetch", t + 27_000, "Agent", "Stripe amount rounding", "Async agent launched successfully."),
      subagent: { id: "a51c2", backend: "claude", kind: "web-fetch", name: null, description: "Stripe amount rounding", prompt: "Find how Stripe expects payout amounts: integer or decimal, and any rounding guidance.", status: "done", startedAt: t + 27_000, endedAt: t + 49_000, toolCalls: 1,
        result: "Stripe amounts are integers in the smallest currency unit (cents). Round before sending; Stripe does not round for you.",
        messages: [subTool("w1", t + 30_000, "WebFetch", "https://docs.stripe.com/currencies#minor-units", "Amounts are integers in the currency's smallest unit."), subSay("w2", t + 49_000, "Stripe amounts are integers in the smallest currency unit (cents). Round before sending; Stripe does not round for you.")], earlier: 0 } },
    subTool("r4", t + 52_000, "Bash", "npx vitest run src/bets/payout.test.ts", " ✓ src/bets/payout.test.ts (4 tests) 6ms\n\n Test Files  1 passed (1)\n      Tests  4 passed (4)"),
    subTool("r5", t + 63_000, "Edit", "src/bets/payout.ts", "-export const payout = (stakeCents: number, odds: number) => stakeCents * odds;\n+export const payout = (stakeCents: number, odds: number) => Math.floor(stakeCents * odds);"),
    subTool("r6", t + 71_000, "Bash", "npx vitest run src/bets", " ✓ src/bets/payout.test.ts (5 tests) 7ms\n ✓ src/bets/settle.test.ts (9 tests) 15ms"),
    subSay("r7", t + 80_000, "Payouts now round down to whole cents; the house keeps the fraction. All 14 bet tests pass."),
  ];
  return [
    subSay("b1", t, a.task + ". Use subagents to research while you build.", "user"),
    subSay("b2", t + 6_000, "I'll send one subagent to map how markets resolve and another to check the payout math, and write the settlement job meanwhile."),
    { ...subTool("toolu_map", t + 10_000, "Agent", "Map market resolution flow", "Async agent launched successfully."),
      subagent: { id: "a8f03", backend: "claude", kind: "Explore", name: null, description: "Map market resolution flow", prompt: "Read-only. Find where a market becomes resolved in this repo and what, if anything, settles the bets on it. Report file:line references and the bet model's fields.", status: "done", startedAt: t + 10_000, endedAt: t + 41_000, toolCalls: 4, result: mapSteps[5].text, messages: mapSteps, earlier: 0 } },
    { ...subTool("toolu_payout", t + 12_000, "Agent", "Review payout rounding", "Async agent launched successfully."),
      get subagent() { return liveSubagent({ id: "a2d71", backend: "claude", kind: "general-purpose", name: null, description: "Review payout rounding", prompt: "Check `src/bets/payout.ts` for rounding errors with decimal odds. Fix them with a test.", finishes: false }, payoutSteps, { start: t + 12_000, every: 5_000 }); } },
    { ...subTool("toolu_old", t + 13_000, "Agent", "Backfill settled bets", "Async agent launched successfully."),
      subagent: { id: "a77e0", backend: "claude", kind: "general-purpose", name: "backfill", description: "Backfill settled bets", prompt: "Write a one-off script that settles bets on markets resolved before today.", status: "stopped", startedAt: t + 13_000, endedAt: t + 33_000, toolCalls: 2, result: null,
        messages: [subTool("k1", t + 20_000, "Bash", "psql $DATABASE_URL -c \"select count(*) from markets where status = 'resolved'\"", " count\n-------\n   318"), subTool("k2", t + 33_000, "Write", "scripts/backfill-settlement.ts", null)], earlier: 0 } },
    subTool("b6", t + 50_000, "Write", "src/jobs/settle.ts", "Wrote 46 lines to src/jobs/settle.ts"),
    subSay("b7", t + 55_000, a.lastMessage),
  ];
}
const SUBAGENT_DEMO = new Set(["run_b1", "run_a7"]);

// MOCK_TRANSCRIPT=<file>: a real /api/app/transcript response (readTranscript
// output) shown as the first agent's conversation.
const realTranscript = process.env.MOCK_TRANSCRIPT ? JSON.parse(fs.readFileSync(process.env.MOCK_TRANSCRIPT, "utf8")) : null;
// MOCK_SLASH_CLAUDE=<file>, MOCK_SLASH_CODEX=<file>: real /api/app/slash-commands
// responses (worker/slash-commands.mjs forRun) for the chat's "/" menu.
const slashLists = Object.fromEntries(["claude", "codex"].filter((b) => process.env[`MOCK_SLASH_${b.toUpperCase()}`])
  .map((b) => [b, JSON.parse(fs.readFileSync(process.env[`MOCK_SLASH_${b.toUpperCase()}`], "utf8"))]));
const transcripts = new Map();
// The agent page's speed and context line (worker/transcripts.mjs `stats`).
function mockStats(a) {
  if (a.fresh || !["claude", "codex"].includes(a.backend)) return null;
  const end = toMsMock(a.updatedAt);
  const rows = a.backend === "codex" ? [[390, 5200], [1210, 16400], [156, 4100], [702, 9800], [1880, 22600]] : [[412, 5100], [1310, 14200], [236, 3900], [884, 9600], [1502, 16800]];
  const responses = rows.map(([tokens, ms], i) => ({ tokens, ms, at: end - (rows.length - 1 - i) * 40_000 }));
  const tokens = responses.reduce((n, r) => n + r.tokens, 0);
  const ms = responses.reduce((n, r) => n + r.ms, 0);
  const speed = { tokensPerSecond: Math.round((tokens / ms) * 10_000) / 10, responses };
  return a.backend === "codex"
    ? { model: a.model, speed, context: { used: 391_400, window: 475_000, modelWindow: 475_000, autoCompactWindow: null, at: end, compactedAt: end - 35 * MIN } }
    : { model: "claude-opus-5-5", speed, context: { used: 153_502, window: 400_000, modelWindow: 1_000_000, autoCompactWindow: 400_000, at: end, compactedAt: null } };
}
const toMsMock = (t) => (typeof t === "number" ? t : Date.parse(t));
function transcriptFor(ws, run) {
  const p = projects.find((x) => x.workspaceId === ws) || (coordinator?.workspaceId === ws ? coordinator : null);
  const a = p?.agents.find((x) => x.id === run);
  if (!a) return null;
  if (!transcripts.has(run) && realTranscript && a === projects[0].agents[0]) transcripts.set(run, realTranscript.messages);
  if (!transcripts.has(run) && SUBAGENT_DEMO.has(run)) transcripts.set(run, subagentTranscript(a));
  if (!transcripts.has(run)) transcripts.set(run, a.fresh ? [{ id: "m1", role: "user", ts: a.createdAt, text: a.task }] : baseTranscript(a));
  return { a, msgs: transcripts.get(run) };
}

function json(res, status, body) {
  res.writeHead(status, { "Content-Type": "application/json", "Cache-Control": "no-store" });
  res.end(JSON.stringify(body));
}
function readBody(req) {
  return new Promise((resolve) => {
    let d = "";
    req.on("data", (c) => { d += c; });
    req.on("end", () => { try { resolve(JSON.parse(d || "{}")); } catch { resolve({}); } });
  });
}
const TYPES = { ".html": "text/html; charset=utf-8", ".js": "text/javascript; charset=utf-8", ".mjs": "text/javascript; charset=utf-8", ".css": "text/css; charset=utf-8", ".svg": "image/svg+xml", ".png": "image/png", ".webmanifest": "application/manifest+json" };

// Built from cloud/src like server.ts serves them (build cloud first).
const COMPILED = { "/app/terminal-recovery.js": "../dist/private-access/terminal-recovery.js", "/app/project-file-render.js": "../dist/project-files/browser-render.js" };
function serveStatic(req, res, url) {
  if (COMPILED[url.pathname]) {
    res.writeHead(200, { "Content-Type": TYPES[".js"], "Cache-Control": "no-cache" });
    return fs.createReadStream(path.join(DIR, COMPILED[url.pathname])).pipe(res);
  }
  let rel = url.pathname.replace(/^\/app\/?/, "");
  const file = path.join(DIR, rel);
  const ok = rel && !rel.includes("..") && rel !== "dev-mock.mjs" && fs.existsSync(file) && fs.statSync(file).isFile();
  const target = ok ? file : path.join(DIR, "index.html");
  const headers = { "Content-Type": TYPES[path.extname(target)] || "application/octet-stream", "Cache-Control": "no-cache" };
  if (rel === "sw.js") headers["Service-Worker-Allowed"] = "/app";
  res.writeHead(200, headers);
  fs.createReadStream(target).pipe(res);
}

// ---------- Team view (cloud/src/team.ts, through its built module) ----------
const teamLib = await import("../dist/team.js").catch(() => null);
const HR = 60 * MIN;
const teammates = {
  "better-auth:sam": { machine: "awake", projects: [
    { workspaceId: "wsp_sam_billing", name: "billing-api", repo: "exla-ai/billing-api", agents: [
      agent({ id: "run_s1", title: "Retry failed invoices with backoff", task: "Retry failed invoices", status: "working", backend: "claude", model: "opus", updatedAt: T0 - 1 * MIN }),
      agent({ id: "run_s2", title: "Salary bands import", task: "secret", status: "waiting", backend: "claude", model: "opus", asks: [{ id: "1-aaaaaaaa", kind: "question" }], visibility: "private", updatedAt: T0 - 6 * MIN }),
      agent({ id: "run_s3", title: "Stripe tax IDs on invoices", task: "Tax IDs", status: "review", backend: "codex", model: "gpt-6.1-sol", updatedAt: T0 - 40 * MIN }),
    ] },
    { workspaceId: "wsp_sam_rudder", name: "rudder", repo: "exla-ai/rudder", agents: [
      agent({ id: "run_s4", title: "Team view in the web app", task: "Show teammates' agents", status: "working", backend: "claude", model: "sonnet", phase: "testing", updatedAt: T0 - 2 * MIN }),
    ] },
  ] },
  "github:991": { machine: "asleep", projects: [
    { workspaceId: "wsp_ava_docs", name: "docs", repo: "exla-ai/docs", agents: [
      agent({ id: "run_v1", title: "Rewrite the quickstart for the Mac app", task: "Quickstart for Mac", status: "done", backend: "claude", model: "opus", updatedAt: T0 - 3 * HR }),
      agent({ id: "run_v2", title: "secret", task: "secret", status: "failed", visibility: "private", updatedAt: T0 - 5 * HR }),
    ] },
  ] },
};
const teamDeps = () => ({
  orgsFor: () => orgList().map((o) => ({ id: o.id, name: o.name })),
  membersOf: (_viewer, orgId) => {
    const org = orgs.get(orgId);
    if (!org || !mine(org)) throw Object.assign(new Error("org not found"), { status: 404 });
    return org.members.map((m) => ({ ...m, you: m.accountId === ME }));
  },
  projectsOf: (id) => teammates[id]?.projects ?? [],
  machineOf: (id) => teammates[id]?.machine ?? "none",
});
function teamRoutes(req, res, p, url, body) {
  if (p === "/api/app/team" && req.method === "GET") {
    if (!teamLib) return json(res, 404, { error: "build cloud first (npm --prefix cloud run build)" }), true;
    try { return json(res, 200, teamLib.buildTeamView({ accountId: ME }, url.searchParams.get("org") || null, teamDeps())), true; }
    catch (err) { return json(res, err.status || 500, { error: err.message }), true; }
  }
  if (p === "/api/app/team/transcript" && req.method === "GET") {
    const mate = teammates[url.searchParams.get("member")];
    const proj = mate?.projects.find((x) => x.workspaceId === url.searchParams.get("workspace"));
    const a = proj?.agents.find((x) => x.id === url.searchParams.get("run"));
    if (!a || a.visibility === "private") return json(res, 404, { error: "not found" }), true;
    if (mate.machine !== "awake") return json(res, 503, { error: "Ava's cloud machine is asleep. Their agents show here again when it wakes.", asleep: true }), true;
    if (!transcripts.has(a.id)) transcripts.set(a.id, baseTranscript(a));
    return json(res, 200, { runId: a.id, title: a.title, backend: a.backend, status: a.status, messages: transcripts.get(a.id), readOnly: true }), true;
  }
  if (p === "/api/app/visibility" && req.method === "POST") {
    const proj = projects.find((x) => x.workspaceId === body.workspace);
    if (!proj) return json(res, 404, { error: "project not found" }), true;
    if (typeof body.newAgentsPrivate === "boolean") { proj.newAgentsPrivate = body.newAgentsPrivate; return json(res, 200, { workspaceId: proj.workspaceId, newAgentsPrivate: proj.newAgentsPrivate }), true; }
    const a = proj.agents.find((x) => x.id === body.run);
    if (!a) return json(res, 404, { error: "agent not found" }), true;
    if (body.visibility !== "team" && body.visibility !== "private") return json(res, 400, { error: "visibility must be team or private" }), true;
    a.visibility = body.visibility;
    return json(res, 200, { workspaceId: proj.workspaceId, run: a.id, visibility: a.visibility }), true;
  }
  return undefined;
}

// ---------- Onboarding, orgs, invites, integrations (CONTRACT.md) ----------
const onboarding = { surfaces: [], completedAt: process.env.MOCK_ONBOARDED === "1" ? iso(now()) : null };
const ME = "better-auth:me";
const orgs = new Map([["org_exla", {
  id: "org_exla", name: "Exla", createdAt: iso(now() - 40 * 24 * 60 * MIN),
  members: [
    { accountId: ME, email: "viraat@example.com", name: "Viraat Das", role: "admin", joinedAt: iso(now() - 40 * 24 * 60 * MIN) },
    { accountId: "better-auth:sam", email: "sam@exla.ai", name: "Sam Lee", role: "admin", joinedAt: iso(now() - 30 * 24 * 60 * MIN) },
    { accountId: "github:991", email: "ava@exla.ai", name: null, role: "member", joinedAt: iso(now() - 3 * 24 * 60 * MIN) },
  ],
  invites: [{ id: "inv_1", email: "jo@exla.ai", role: "member", createdAt: iso(now() - 2 * 60 * MIN), expiresAt: iso(now() + 6 * 24 * 60 * MIN) }],
}]]);
const inviteTokens = new Map([["tok_demo", { orgId: "org_exla", role: "member", email: null, used: false }]]);
let integMode = "on"; // on: configured | off: configured, nothing connected | unset: configured:false
const integStore = new Map();
function integ(scope) {
  if (!integStore.has(scope)) integStore.set(scope, scope === "org_exla"
    ? { slack: { connected: true, team: { id: "T1", name: "Exla" }, channel: { id: "C2", name: "agents" } } }
    : { slack: { connected: false, team: null, channel: null } });
  return integStore.get(scope);
}
const mine = (o) => o.members.find((m) => m.accountId === ME);
const orgList = () => [...orgs.values()].filter(mine).map((o) => ({ id: o.id, name: o.name, role: mine(o).role, memberCount: o.members.length, createdAt: o.createdAt }));

/** Returns true when it answered the request, undefined otherwise. */
function orgRoutes(req, res, p, url, body) {
  const m = req.method;
  const send = (status, obj) => { if (status === 204) { res.writeHead(204); res.end(); } else json(res, status, obj); return true; };
  if (p === "/api/app/event" && m === "POST") return send(204);
  if (p === "/api/app/onboarding") {
    if (m === "POST") {
      if (Array.isArray(body.surfaces)) onboarding.surfaces = body.surfaces.filter((s) => ["web", "mac", "terminal", "phone"].includes(s));
      if (body.complete) onboarding.completedAt = iso(now());
    }
    return send(200, { ...onboarding });
  }
  if (p === "/api/app/orgs") {
    if (m === "POST") {
      const name = String(body.name || "").trim();
      if (!name) return send(400, { error: "Give the organization a name." });
      const id = `org_${Math.random().toString(36).slice(2, 8)}`;
      orgs.set(id, { id, name, createdAt: iso(now()), members: [{ accountId: ME, email: "viraat@example.com", name: "Viraat Das", role: "admin", joinedAt: iso(now()) }], invites: [] });
      return send(200, { org: { id, name, role: "admin", memberCount: 1, createdAt: iso(now()) } });
    }
    return send(200, { orgs: orgList() });
  }
  let r;
  if ((r = p.match(/^\/api\/app\/orgs\/([^/]+)(?:\/(invites|members)(?:\/([^/]+))?)?$/))) {
    const org = orgs.get(decodeURIComponent(r[1]));
    if (!org || !mine(org)) return send(404, { error: "Organization not found" });
    const admin = mine(org).role === "admin";
    const admins = () => org.members.filter((x) => x.role === "admin").length;
    const [, , sub, rawId] = r;
    const id = rawId && decodeURIComponent(rawId);
    if (!sub) {
      if (m === "GET") return send(200, { org: { id: org.id, name: org.name, createdAt: org.createdAt }, role: mine(org).role, members: org.members.map((x) => ({ ...x, you: x.accountId === ME })), invites: admin ? org.invites : [] });
      if (!admin) return send(403, { error: "Only admins can do that." });
      if (m === "PATCH") { org.name = String(body.name || org.name).trim(); return send(200, { org: { id: org.id, name: org.name } }); }
      if (m === "DELETE") { orgs.delete(org.id); return send(200, { ok: true }); }
    }
    if (sub === "invites") {
      if (!admin) return send(403, { error: "Only admins can do that." });
      if (m === "POST") {
        const token = `tok_${Math.random().toString(36).slice(2, 12)}`;
        const inv = { id: `inv_${Math.random().toString(36).slice(2, 8)}`, email: body.email || null, role: body.role === "admin" ? "admin" : "member", createdAt: iso(now()), expiresAt: iso(now() + 7 * 24 * 60 * MIN) };
        org.invites.push(inv);
        inviteTokens.set(token, { orgId: org.id, role: inv.role, email: inv.email, used: false, inviteId: inv.id });
        return send(200, { invite: { ...inv, url: `http://${req.headers.host}/app/join/${token}` } });
      }
      if (m === "DELETE" && id) { org.invites = org.invites.filter((i) => i.id !== id); return send(200, { ok: true }); }
    }
    if (sub === "members" && id) {
      const target = org.members.find((x) => x.accountId === id);
      if (!target) return send(404, { error: "Not a member" });
      if (m === "POST") {
        if (!admin) return send(403, { error: "Only admins can do that." });
        if (target.role === "admin" && body.role !== "admin" && admins() === 1) return send(409, { error: "an org needs at least one admin" });
        target.role = body.role === "admin" ? "admin" : "member";
        return send(200, { ok: true });
      }
      if (m === "DELETE") {
        if (!admin && id !== ME) return send(403, { error: "Only admins can do that." });
        if (target.role === "admin" && admins() === 1) return send(409, { error: "an org needs at least one admin" });
        org.members = org.members.filter((x) => x !== target);
        return send(200, { ok: true });
      }
    }
    return send(405, { error: "Method not allowed" });
  }
  if ((r = p.match(/^\/api\/app\/invites\/([^/]+)(\/accept)?$/))) {
    const inv = inviteTokens.get(decodeURIComponent(r[1]));
    if (!inv) return send(404, { error: "This invite link is not valid." });
    if (inv.used) return send(410, { error: "This invite was already used or has expired." });
    const org = orgs.get(inv.orgId);
    if (!org) return send(410, { error: "This invite was already used or has expired." });
    const alreadyMember = !!mine(org);
    if (!r[2]) return send(200, { org: { id: org.id, name: org.name }, role: inv.role, email: inv.email, expiresAt: iso(now() + 6 * 24 * 60 * MIN), alreadyMember });
    if (m !== "POST") return send(405, { error: "Method not allowed" });
    if (inv.email && inv.email !== "viraat@example.com") return send(403, { error: `This invite is for ${inv.email}.` });
    if (!alreadyMember) org.members.push({ accountId: ME, email: "viraat@example.com", name: "Viraat Das", role: inv.role, joinedAt: iso(now()) });
    inv.used = true;
    if (inv.inviteId) org.invites = org.invites.filter((i) => i.id !== inv.inviteId);
    return send(200, { org: { id: org.id, name: org.name, role: mine(org).role } });
  }
  const scope = url.searchParams.get("org") || body.org || "personal";
  const orgOk = scope === "personal" || (orgs.get(scope) && mine(orgs.get(scope)));
  const canManage = scope === "personal" || mine(orgs.get(scope) || { members: [] })?.role === "admin";
  if (p.startsWith("/api/app/integrations")) {
    if (!orgOk) return send(404, { error: "Organization not found" });
  }
  const configured = integMode !== "unset";
  if (p === "/api/app/integrations" && m === "GET") {
    const it = integ(scope);
    const on = integMode === "on";
    return send(200, {
      slack: { configured, connected: configured && on && it.slack.connected, team: on ? it.slack.team : null, channel: on ? it.slack.channel : null },
      canManage,
    });
  }
  if ((r = p.match(/^\/api\/app\/integrations\/(slack)\/start$/)) && m === "POST") {
    if (!configured) return send(503, { error: "Slack is not set up on this server yet." });
    if (!canManage) return send(403, { error: "Only admins can do that." });
    integMode = "on";
    return send(200, { url: `/app/__mock/oauth/${r[1]}${scope !== "personal" ? `?org=${encodeURIComponent(scope)}` : ""}` });
  }
  if (p === "/api/app/integrations/slack" && m === "DELETE") {
    if (!canManage) return send(403, { error: "Only admins can do that." });
    integ(scope).slack = { connected: false, team: null, channel: null };
    return send(200, { ok: true });
  }
  const CHANNELS = [{ id: "C1", name: "general" }, { id: "C2", name: "agents" }, { id: "C3", name: "eng-alerts" }];
  if (p === "/api/app/integrations/slack/channels" && m === "GET") return send(200, { channels: CHANNELS });
  if (p === "/api/app/integrations/slack/channel" && m === "POST") {
    const ch = CHANNELS.find((c) => c.id === body.channel);
    if (!ch) return send(400, { error: "Pick a channel." });
    integ(scope).slack.channel = ch;
    return send(200, { channel: ch });
  }
  if ((r = p.match(/^\/api\/app\/link\/slack\/([^/]+)$/)) && m === "POST") {
    if (r[1] === "expired") return send(410, { error: "This link has expired. Run /rudder link in Slack again." });
    return send(200, { ok: true, slack: { team: "Exla", user: "viraat" } });
  }
  return undefined;
}

const delay = (ms) => new Promise((r) => setTimeout(r, ms));

// ---------- Project files (files-panel.js) ----------
// Two workspaces of wsp_rudder: the main checkout and run_a1's own fork.
const PNG_DOT = "iVBORw0KGgoAAAANSUhEUgAAAPAAAABaCAIAAAAJsExNAAAB3klEQVR4nO3dsW0DQRRDwevI3bgIZUpcq7twYihW7PgILE0M8ApYfE6+1+v3R5rpOv4CKRjQmgpoTQW0pgJaUwGtqYDWVEBrKqA1FdCaCmhNBbSmAlpTAa2pgNZUQGsqoDUV0JoKaE0FtKYCWlMBramA1lRAayqgNdVd0B+f30sd38MKN1cAOnZKoBtWADp2SqAbVgA6dkqgG1YAOnZKoBtWADp2SqAbVgA6dkqgG1YAOnZKoBtWADp2SqAbVgA6dkqgG1YAOnZKoBtWADp2SqAbVgA6dkqgG1YAOnZKoBtWADp2SqAbVgA6dkqgG1YAOnZKoBtWADp2SqAbVgD6T1/Px3/s+N2yAR3rOE2ggU52nCbQQLec8mDH79azAtCxUwLdsALQsVMC3bAC0LFTAt2wAtCxUwLdsALQsVMC3bAC0LFTAt2wAtCxUwLdsALQsVMC3bAC0LFTAt2wAtCxUwLdsALQsVMC3bAC0LFTAt2wAtCxUwLdsALQsVMC3bAC0LFTAt2wAtCxUwLdsALQsVMC3bCCfwo1FdCaCmhNBbSmAlpTAa2pgNZUQGsqoDUV0JoKaE0FtKYCWlMBramA1lRAayqgNRXQmgpoTQW0pgJaUwGtqYDWVEBrKqA11RsfFgRxnxmPjgAAAABJRU5ErkJggg==";
const readmeText = (extra = "") => `# rudder\n\nRudder runs Claude Code and Codex agents on your own machines. ${extra}\n\n![Architecture](docs/diagram.png)\n\n## Layout\n\n| Path | What it holds |\n|---|---|\n| \`src/\` | the CLI |\n| \`native/\` | the Rust dashboard |\n| \`cloud/\` | control plane and workers |\n\n## Try it\n\n\`\`\`sh\nnpm install -g @exla/rudder\nrudder "fix the flaky test"\n\`\`\`\n\nSee the [guide](docs/guide.md) for more.\n`;
const mainTs = `import { runManager } from "./run-manager.js";\n\n// Start one agent and print where it runs.\nexport async function main(argv: string[]): Promise<number> {\n  const task = argv.join(" ").trim();\n  if (!task) {\n    console.error("Usage: rudder \\"task\\"");\n    return 2;\n  }\n  const run = await runManager.start({ task, backend: "claude" });\n  console.log(\`Started \${run.id} in \${run.workspace.path}\`);\n  return 0;\n}\n`;
const fileTrees = {
  checkout: {
    "README.md": readmeText(),
    "AGENTS.md": "# Rudder engineering rules\n\nRudder runs Claude Code and Codex agents locally and on Fly.io Linux.\n\n## Verification\n\n- Run one targeted check for what you changed.\n- No full suites per change.\n",
    "docs/guide.md": "# Guide\n\nStart with the [README](../README.md).\n\n- Agents work in their own workspace.\n- Finished work merges into main.\n",
    "docs/diagram.png": Buffer.from(PNG_DOT, "base64"),
    "src/main.ts": mainTs,
    "src/run-manager.ts": "export const runManager = {\n  async start(options: { task: string; backend: string }) {\n    return { id: \"run-1\", workspace: { path: \"/tmp\" }, ...options };\n  },\n};\n",
    "Cargo.toml": "[workspace]\nmembers = [\"native\", \"engine\"]\nresolver = \"2\"\n",
    "package.json": "{\n  \"name\": \"@exla/rudder\",\n  \"version\": \"4.4.2\",\n  \"type\": \"module\"\n}\n",
  },
};
fileTrees.run_a1 = { ...fileTrees.checkout, "README.md": readmeText("This line is the agent's own edit."), "src/files-panel.ts": "// Added by the agent in its own workspace.\nexport const panel = true;\n" };
const fileScopes = (ws) => ws !== "wsp_rudder" ? [{ projectId: "prj_mock", workspaceId: "checkout", attemptId: "checkout", ownerEpoch: 3 }]
  : ["checkout", "run_a1"].map((id) => ({ projectId: "prj_rudder", workspaceId: id, attemptId: id === "checkout" ? id : `${id}-attempt-1`, ownerEpoch: 3 }));
const revisionOf = (value) => createHash("sha256").update(value).digest("hex");
function fileDoc(rel, value, known) {
  const ext = path.extname(rel).toLowerCase();
  const revision = revisionOf(value);
  const base = { path: rel, revision };
  if (ext === ".png") return { ...base, kind: "image", mime: "image/png", ...(known === revision ? { unchanged: true } : { base64: value.toString("base64") }) };
  const kind = ext === ".md" ? "markdown" : ext === ".html" ? "html" : "text";
  const mime = kind === "markdown" ? "text/markdown" : kind === "html" ? "text/html" : ext === ".css" ? "text/css" : "text/plain";
  return { ...base, kind, mime, ...(known === revision ? { unchanged: true } : { text: value }) };
}
function projectFiles(req, res, url, body) {
  const q = body ?? Object.fromEntries(url.searchParams);
  const scope = body ? body.scope : { workspaceId: q.workspaceId };
  const tree = fileTrees[scope?.workspaceId] ?? (scope?.workspaceId === "checkout" ? fileTrees.checkout : null);
  if (!tree) return json(res, 403, { error: "Refresh this project's file workspaces" });
  return treeFiles(res, tree, q, body);
}
function treeFiles(res, tree, q, body) {
  const rel = q.path ?? "";
  if (body) {
    const current = tree[rel];
    if (typeof current !== "string") return json(res, 404, { error: "The project file no longer exists" });
    if (revisionOf(current) !== body.baseRevision) return json(res, 200, { path: rel, outcome: "conflict" });
    tree[rel] = body.text;
    return json(res, 200, { path: rel, outcome: "saved", revision: revisionOf(body.text) });
  }
  if (q.operation === "list") {
    const prefix = rel ? `${rel}/` : "";
    const seen = new Map();
    for (const key of Object.keys(tree)) {
      if (!key.startsWith(prefix)) continue;
      const [name, ...rest] = key.slice(prefix.length).split("/");
      seen.set(name, { name, path: prefix + name, kind: rest.length ? "folder" : "file" });
    }
    const entries = [...seen.values()].sort((a, b) => a.kind === b.kind ? a.name.localeCompare(b.name) : a.kind === "folder" ? -1 : 1);
    return json(res, 200, { path: rel, entries, truncated: false });
  }
  const value = tree[rel];
  if (value === undefined) return json(res, 404, { error: "The project file no longer exists" });
  return json(res, 200, fileDoc(rel, value, q.knownRevision));
}

// ---------- Agent setup (files-hub.js) ----------
// Separate instruction files at first, so the page offers "Use one file for both".
const skillText = (name, description, body) => `---\nname: ${name}\ndescription: ${description}\n---\n\n# ${name}\n\n${body}\n`;
const freshSetup = () => ({
  linked: false,
  roots: {
    "instructions:claude": { "CLAUDE.md": "# Rules for every agent\n\n- No fallbacks unless asked.\n- Links to things running here go over Tailscale.\n- Run one targeted check per change.\n" },
    "instructions:codex": { "AGENTS.md": "# Codex notes\n\n- Prefer small commits on main.\n" },
    "skill:claude:rudder-burst": { "SKILL.md": skillText("rudder-burst", "Run heavy builds on the burst build pool instead of the shared home", "Use `rudder-burst <command>`.") },
    "skill:codex:rudder-burst": { "SKILL.md": skillText("rudder-burst", "Run heavy builds on the burst build pool instead of the shared home", "Use `rudder-burst <command>` (older copy).") },
    "skill:claude:yc": { "SKILL.md": skillText("yc", "Search Bookface and YC knowledge from the terminal", "Run `yc search`."), "reference/commands.md": "# Commands\n" },
    "skill:codex:libra-investigate": { "SKILL.md": skillText("libra-investigate", "Investigate a production alert with Libra's telemetry", "Start from the alert.") },
    "folder:claude-agents": { "reviewer.md": "---\nname: reviewer\ndescription: Reviews a diff for bugs\n---\nReview the diff.\n" },
  },
  shared: [],   // names of skills in ~/.agents/skills
  sharedWith: {}, // name -> true when linked into the other agent
});
let setup = freshSetup();
const setupTree = (id) => (id === "instructions:codex" && setup.linked ? setup.roots["instructions:claude"] : setup.roots[id]);
function setupOverview() {
  const claudeFile = setup.roots["instructions:claude"]["CLAUDE.md"] !== undefined;
  const skills = [];
  const row = (name, agents, root, location, note = "") => {
    const meta = /description: (.*)/.exec(setupTree(root)?.["SKILL.md"] ?? "");
    skills.push({ name, description: meta?.[1] ?? "", agents, root, location, note });
  };
  row("rudder-burst", ["claude"], "skill:claude:rudder-burst", "~/.claude/skills/rudder-burst", "Each agent has its own copy of rudder-burst. Edits to one do not reach the other.");
  row("rudder-burst", ["codex"], "skill:codex:rudder-burst", "~/.codex/skills/rudder-burst", "Each agent has its own copy of rudder-burst. Edits to one do not reach the other.");
  row("yc", setup.sharedWith.yc ? ["claude", "codex"] : ["claude"], "skill:claude:yc", "~/.claude/skills/yc");
  row("libra-investigate", ["codex"], "skill:codex:libra-investigate", "~/workspace/code/libra/.agents/skills/libra-investigate");
  for (const name of setup.shared) row(name, ["claude", "codex"], `skill:shared:${name}`, `~/.agents/skills/${name}`);
  skills.sort((a, b) => a.name.localeCompare(b.name));
  return {
    agents: { claude: { dir: "~/.claude", present: true }, codex: { dir: "~/.codex", present: true } },
    instructions: {
      claude: { agent: "claude", path: "~/.claude/CLAUDE.md", state: claudeFile ? "file" : "missing", linksTo: null, root: "instructions:claude" },
      codex: { agent: "codex", path: "~/.codex/AGENTS.md", state: setup.linked ? "link" : "file", linksTo: setup.linked ? "~/.claude/CLAUDE.md" : null, root: "instructions:codex" },
      shared: setup.linked,
    },
    skills,
    folders: [{ id: "folder:claude-agents", label: "Claude subagents", root: "folder:claude-agents" }],
  };
}
function setupAction(res, body) {
  if (body.operation === "link-instructions") {
    const replaced = setup.roots["instructions:codex"]["AGENTS.md"];
    if (!setup.linked && body.keep === "claude" && revisionOf(replaced) !== body.replacedRevision) return json(res, 409, { error: "~/.codex/AGENTS.md changed since you looked at it. Nothing was linked." });
    if (body.keep === "codex") setup.roots["instructions:claude"] = { "CLAUDE.md": replaced };
    setup.linked = true;
  } else if (body.operation === "share-skill") {
    setup.sharedWith[body.name] = true;
  } else if (body.operation === "new-skill") {
    setup.shared.push(body.name);
    setup.roots[`skill:shared:${body.name}`] = { "SKILL.md": skillText(body.name, body.description, "What the agent should do, step by step.") };
  }
  return json(res, 200, setupOverview());
}
const projectGuides = {
  wsp_rudder: { files: [
    { path: "AGENTS.md", kind: "file", linksTo: null, imports: [], readBy: ["claude", "codex"] },
    { path: "CLAUDE.md", kind: "link", linksTo: "AGENTS.md", imports: [], readBy: ["claude", "codex"] },
    { path: "README.md", kind: "file", linksTo: null, imports: [], readBy: [] },
  ], shared: true },
};

// ---------- Vibeprint canvas (canvas-panel.js) ----------
// A real `vibeprint serve` stands in for the home's: MOCK_VIBEPRINT_URL is its
// page (http://127.0.0.1:PORT/) and MOCK_VIBEPRINT_TOKEN its access token. The
// eye's check sends it a made-up diff for the agent, as the home sends the real one.
const vibeprintOn = new Map(); // ws -> start time
const vibeprintTracked = new Map(); // `${ws}/${run}` -> { lastAt }
async function vibeprintMock(req, res, url) {
  const body = req.method === "POST" ? await readBody(req) : null;
  const ws = body ? body.workspace : url.searchParams.get("workspace");
  const run = body ? body.run : url.searchParams.get("run");
  const base = process.env.MOCK_VIBEPRINT_URL, token = process.env.MOCK_VIBEPRINT_TOKEN;
  const tracking = () => {
    const t = vibeprintTracked.get(`${ws}/${run}`);
    return t ? { on: true, ...(t.lastAt ? { lastAt: new Date(t.lastAt).toISOString() } : {}) } : { on: false };
  };
  const view = () => {
    const since = vibeprintOn.get(ws);
    const t = run ? { tracking: tracking() } : {};
    if (since === undefined) return { state: "off", ...t };
    if (!base || !token) return { state: "failed", error: "Set MOCK_VIBEPRINT_URL and MOCK_VIBEPRINT_TOKEN to a running vibeprint serve.", ...t };
    return now() - since < 1500 ? { state: "starting", ...t } : { state: "running", url: base, token, ...t };
  };
  const vp = (route, payload) => fetch(`${base}${route}`, { method: payload ? "POST" : "GET", headers: { "x-vibeprint-token": token, "content-type": "application/json" }, ...(payload ? { body: JSON.stringify(payload) } : {}) });
  if (!body) return json(res, 200, view());
  if (body.op === "start") { if (!vibeprintOn.has(ws)) vibeprintOn.set(ws, now()); return json(res, 200, view()); }
  if (body.op === "stop") { vibeprintOn.delete(ws); return json(res, 200, { ...view(), state: "off" }); }
  if (body.op === "tracking") {
    if (!body.on) { vibeprintTracked.delete(`${ws}/${run}`); return json(res, 200, view()); }
    const entry = vibeprintTracked.get(`${ws}/${run}`) ?? {};
    vibeprintTracked.set(`${ws}/${run}`, entry);
    if (body.check) {
      const agent = projects.find((p) => p.workspaceId === ws)?.agents.find((a) => a.id === run);
      const r = await vp("api/track", { author: agent?.title ?? run, task: agent?.task ?? "", diff: "diff --git a/api/items.js b/api/items.js\n+export const items = (db) => db.query('select * from items')\n" });
      if (r.ok) entry.lastAt = now();
    }
    return json(res, 200, view());
  }
  if (body.op === "brief") {
    if (view().state !== "running") return json(res, 409, { error: "The canvas is not running" });
    const r = await vp(`api/brief${body.since ? `?since=${encodeURIComponent(body.since)}` : ""}`);
    return json(res, r.status, await r.json());
  }
  return json(res, 400, { error: "Unknown canvas request" });
}

const server = http.createServer(async (req, res) => {
  const url = new URL(req.url, `http://${req.headers.host}`);
  const p = url.pathname;

  if (p === "/") { res.writeHead(302, { Location: "/app" }); return res.end(); }
  if (p.startsWith("/app/login")) {
    signedOut = false;
    const next = url.searchParams.get("next") || "";
    res.writeHead(302, { Location: /^\/app\/[\w\-/%.]*$/.test(next) ? next : "/app" });
    return res.end();
  }
  if (p === "/app/__mock/signout") { signedOut = true; res.writeHead(302, { Location: "/app" }); return res.end(); }
  if (p.startsWith("/app/__mock/home/")) { homeMode = p.split("/").pop(); res.writeHead(302, { Location: "/app" }); return res.end(); }
  if (p.startsWith("/app/__mock/usage/")) { usageMode = p.split("/").pop(); res.writeHead(302, { Location: "/app" }); return res.end(); }
  if (p.startsWith("/app/__mock/creds/")) {
    creds = (CREDS[p.split("/").pop()] || CREDS.out)();
    for (const key of Object.keys(logins)) delete logins[key];
    res.writeHead(302, { Location: "/app" });
    return res.end();
  }
  if (p === "/app/__mock/fresh") {
    creds = CREDS.out();
    githubConnected = false;
    freshProjects = [];
    res.writeHead(302, { Location: "/app" });
    return res.end();
  }
  if (p === "/app/__mock/computer") {
    const code = `K${Math.floor(Math.random() * 8) + 2}P-${Math.random().toString(36).slice(2, 5).toUpperCase().replace(/[01OIL]/g, "X")}`;
    computers.set(code, { deviceName: "Viraats-MacBook-Pro.local", expiresAt: now() + 5 * MIN });
    res.writeHead(200, { "Content-Type": "text/plain" });
    return res.end(`code: ${code}\napprove: /app/approve?code=${code}\n`);
  }
  if (p === "/app/__mock/files/reset-setup") {
    setup = freshSetup();
    res.writeHead(200, { "Content-Type": "text/plain" });
    return res.end("agent setup reset\n");
  }
  if (p === "/app/__mock/files/agent-edit") {
    fileTrees.run_a1["README.md"] = readmeText(`The agent rewrote this at ${new Date().toLocaleTimeString()}.`);
    res.writeHead(200, { "Content-Type": "text/plain" });
    return res.end("run_a1 changed README.md\n");
  }
  if (p === "/app/__mock/onboard") { onboarding.completedAt = null; res.writeHead(302, { Location: "/app" }); return res.end(); }
  if (p.startsWith("/app/__mock/integrations/")) { integMode = p.split("/").pop(); res.writeHead(302, { Location: "/app/settings" }); return res.end(); }
  if (p === "/app/__mock/invite") {
    const token = `tok_${Math.random().toString(36).slice(2, 10)}`;
    inviteTokens.set(token, { orgId: "org_exla", role: "member", email: null, used: false });
    res.writeHead(200, { "Content-Type": "text/plain" });
    return res.end(`join: /app/join/${token}\n`);
  }
  if (p.startsWith("/app/__mock/oauth/")) {
    const provider = p.split("/").pop();
    const scope = url.searchParams.get("org") || "personal";
    const it = integ(scope);
    if (provider === "slack") it.slack = { ...it.slack, connected: true, team: { id: "T1", name: "Exla" }, channel: null };
    res.writeHead(302, { Location: `/app/settings?connected=${provider}` });
    return res.end();
  }
  if (p === "/api/auth/sign-out" && req.method === "POST") { signedOut = true; return json(res, 200, { success: true }); }

  if (p.startsWith("/api/app/")) {
    await delay(120 + Math.random() * 180);
    if (signedOut) return json(res, 401, { error: "Not signed in" });

    if (p === "/api/app/state" && req.method === "GET") {
      // Living data: the working agent's message ticks forward.
      const a1 = projects[0].agents[0];
      a1.updatedAt = now() - 20_000;
      return json(res, 200, {
        email: "viraat@example.com", home: home(),
        projects: homeMode === "none" ? [] : freshProjects ?? projects,
        coordinator,
        github: { connected: githubConnected, source: githubConnected ? "sync" : null, updatedAt: null },
        digest: digestPreviewOf(digestStore[0]) ?? null,
        onboarding: { ...onboarding },
        orgs: orgList(),
      });
    }
    if (p === "/api/app/project-file-scopes" && req.method === "GET") {
      return json(res, 200, { scopes: fileScopes(url.searchParams.get("workspaceId")) });
    }
    if (p === "/api/app/project-files") {
      return projectFiles(req, res, url, req.method === "POST" ? await readBody(req) : null);
    }
    if (p === "/api/app/vibeprint") return vibeprintMock(req, res, url);
    if (p === "/api/app/agent-setup" && req.method === "GET") return json(res, 200, setupOverview());
    if (p === "/api/app/agent-setup/actions" && req.method === "POST") return setupAction(res, await readBody(req));
    if (p === "/api/app/agent-setup/files") {
      const body = req.method === "POST" ? await readBody(req) : null;
      const tree = setupTree(body ? body.scope?.workspaceId : url.searchParams.get("workspaceId"));
      if (!tree) return json(res, 403, { error: "That agent setup file is not available" });
      return treeFiles(res, tree, body ?? Object.fromEntries(url.searchParams), body);
    }
    if (p === "/api/app/project-guide" && req.method === "GET") {
      return json(res, 200, projectGuides[url.searchParams.get("workspaceId")] ?? { files: [{ path: "README.md", kind: "file", linksTo: null, imports: [], readBy: [] }], shared: false });
    }
    if (p === "/api/app/digests" && req.method === "GET") {
      return json(res, 200, { digests: digestStore.map(digestPreviewOf), timeZone: "America/Los_Angeles" });
    }
    if (p === "/api/app/digest" && req.method === "GET") {
      if (url.searchParams.get("live")) {
        const live = mockDigest(digestDay(now()), now(), true);
        return live ? json(res, 200, { id: null, live: true, ...live, since: morning }) : json(res, 404, { error: "build cloud first" });
      }
      const d = digestStore.find((x) => x.id === url.searchParams.get("id")) ?? (url.searchParams.get("id") ? null : digestStore[0]);
      return d ? json(res, 200, { id: d.id, readAt: d.readAt, createdAt: d.createdAt, ...d.body }) : json(res, 404, { error: "no digest yet" });
    }
    if (p === "/api/app/digest/read" && req.method === "POST") {
      const body = await readBody(req);
      const d = digestStore.find((x) => x.id === body.id);
      if (d && !d.readAt) d.readAt = iso(now());
      return json(res, 200, { ok: true });
    }
    if (p === "/api/app/slash-commands" && req.method === "GET") {
      const t = transcriptFor(url.searchParams.get("workspace"), url.searchParams.get("run"));
      if (!t) return json(res, 404, { error: "Agent not found" });
      const list = slashLists[t.a.backend];
      if (!list) return json(res, 500, { error: `no "/" list for ${t.a.backend} agents in the mock (MOCK_SLASH_${t.a.backend.toUpperCase()})` });
      return json(res, 200, list);
    }
    if (p === "/api/app/transcript" && req.method === "GET") {
      const t = transcriptFor(url.searchParams.get("workspace"), url.searchParams.get("run"));
      if (!t) return json(res, 404, { error: "Agent not found" });
      const limit = Number(url.searchParams.get("limit") || 300);
      return json(res, 200, { runId: t.a.id, title: t.a.title, backend: t.a.backend, status: t.a.status, truncated: t.msgs.length > limit, messages: t.msgs.slice(-limit), stats: mockStats(t.a) });
    }
    // Plan usage for the sidebar meter (src/usage.ts QuotaReport, from the home).
    if (p === "/api/app/usage" && req.method === "GET") {
      if (homeMode !== "running") return json(res, 503, { error: "The cloud machine is asleep", asleep: true });
      if (usageMode === "old") return json(res, 400, { error: "Your cloud machine runs Rudder 4.4.2, which does not have this yet. Update it: press Alt+r in its dashboard." });
      const fetchedAt = iso(now() - 2 * MIN);
      const win = (label, percent, inMin) => ({ label, percent, resetsAt: iso(now() + inMin * MIN) });
      return json(res, 200, { fetchedAt, ttlSeconds: 300, accounts: [
        { provider: "claude", email: "viraat@example.com", plan: "Max 20x", blocked: null, error: null, fetchedAt,
          windows: [win("Session (5h)", 42, 134), win("Weekly", 61, 3 * 24 * 60 + 200), win("Weekly · Opus", 74, 3 * 24 * 60 + 200)] },
        { provider: "codex", email: "viraat@example.com", plan: "Pro", blocked: null, error: null, fetchedAt,
          windows: [win("Session (5h)", 18, 251), win("Weekly", 33, 5 * 24 * 60 + 30)] },
      ] });
    }
    if (p === "/api/app/upload" && req.method === "POST") {
      let size = 0;
      for await (const chunk of req) size += chunk.length;
      const name = String(url.searchParams.get("name") || "file").replace(/[^A-Za-z0-9._-]+/g, "-");
      const proj = projects.find((x) => x.workspaceId === url.searchParams.get("workspace"));
      if (!proj) return json(res, 404, { error: "Project not found" });
      if (/fail/i.test(name)) return json(res, 502, { error: "could not fetch the file (HTTP 500)" });
      await delay(700);
      return json(res, 200, { path: `/workspace/code/${proj.name}/.rudder/uploads/20261017-013000-${name}`, name, size });
    }
    const body = req.method === "POST" || req.method === "PATCH" ? await readBody(req) : {};
    const handled = orgRoutes(req, res, p, url, body) ?? teamRoutes(req, res, p, url, body);
    if (handled !== undefined) return;
    if (p === "/api/app/send") {
      const t = transcriptFor(body.workspace, body.run);
      if (!t) return json(res, 404, { error: "Agent not found" });
      if (/fail/i.test(body.text || "")) return json(res, 502, { error: "The agent's terminal is not accepting input right now." });
      setTimeout(() => {
        t.msgs.push({ id: `u${now()}`, role: "user", ts: now(), text: body.text });
        t.a.status = "working"; t.a.updatedAt = now();
      }, 2500);
      setTimeout(() => {
        t.msgs.push({ id: `tl${now()}`, role: "tool", ts: now(), text: "", tool: { name: "Bash", input: "git status --short", output: " M cloud/app/app.js\n M cloud/app/styles.css", isError: false } });
      }, 5000);
      setTimeout(() => {
        t.msgs.push({ id: `a${now()}`, role: "assistant", ts: now(), text: `On it. I read your note:\n\n> ${String(body.text).slice(0, 200)}\n\nI'll report back when the change is in.` });
        t.a.status = "review"; t.a.updatedAt = now(); t.a.lastMessage = "On it. I'll report back when the change is in.";
      }, 8000);
      return json(res, 200, { ok: true });
    }
    if (p === "/api/app/coordinator") {
      coordinator ??= { workspaceId: "wsp_all_agents", name: "rudder-all-agents", repo: "", status: "running", dashboard: "running", agents: [], lastActivityAt: iso(now()) };
      return json(res, 200, { workspace: coordinator.workspaceId });
    }
    if (p === "/api/app/task" && body.workspace === coordinator?.workspaceId) {
      const main = /^\/main\s+([\s\S]+)/.exec(String(body.text || ""));
      if (!main) return json(res, 200, { ok: true, status: "delivered" }); // /model
      const id = `run_all_${Math.random().toString(36).slice(2, 6)}`;
      const a = agent({ id, title: main[1].slice(0, 60), task: main[1], status: "working", backend: "claude", model: "claude-sonnet-5-5", mode: "main", createdAt: now(), updatedAt: now(), fresh: true });
      setTimeout(() => coordinator.agents.unshift(a), 900);
      setTimeout(() => {
        transcriptFor(coordinator.workspaceId, id)?.msgs.push({ id: `t${now()}`, role: "tool", ts: now(), text: "", tool: { name: "mcp__all_agents__list_agents", input: "{}", output: "rudder [wsp_rudder]\n- run_a1 · working · claude opus · 1m ago · Phone app for agent control\n- run_a2 · waiting · codex gpt-6.1-sol · 4m ago · Fix flaky Ctrl+W chord test\n  needs you: permission for Bash: cargo test --release (prompt ask_1)", isError: false } });
      }, 2500);
      setTimeout(() => {
        transcriptFor(coordinator.workspaceId, id)?.msgs.push({ id: `u${now()}`, role: "tool", ts: now(), text: "", tool: { name: "mcp__all_agents__read_agent", input: "{\"agent\":\"run_a2\",\"project\":\"rudder\"}", output: "rudder / Fix flaky Ctrl+W chord test (run_a2)\nstatus: waiting · codex gpt-6.1-sol · updated 4m ago\nneeds you: permission for Bash: cargo test --release (prompt ask_1)\n\n[agent] The chord test passes alone and fails in the full run. Running the release suite to confirm.", isError: false } });
      }, 3500);
      setTimeout(() => {
        transcriptFor(coordinator.workspaceId, id)?.msgs.push({ id: `r${now()}`, role: "assistant", ts: now(), text: "Here is where things stand:\n\n**rudder**\n- *Phone app for agent control* is working: wiring the transcript poll.\n- *Fix flaky Ctrl+W chord test* needs you: it wants to run `cargo test --release` with network access.\n- *Tailnet HTTPS for previews* is done and ready to merge.\n\nWant me to approve the test run, or keep an eye on the working agents and tell you when they finish?" });
        a.status = "done"; a.updatedAt = now(); a.lastMessage = "Here is where things stand.";
      }, 6000);
      return json(res, 200, { ok: true, status: "delivered" });
    }
    if (p === "/api/app/task") {
      const proj = projects.find((x) => x.workspaceId === body.workspace);
      if (!proj) return json(res, 404, { error: "Project not found" });
      const id = `run_${Math.random().toString(36).slice(2, 8)}`;
      const title = String(body.text || "New task").split(/[.\n]/)[0].slice(0, 60);
      proj.agents.unshift(agent({ id, title, task: body.text, status: "working", backend: "claude", model: "opus", mode: "main", createdAt: now(), updatedAt: now(), lastMessage: "Starting up.", fresh: true }));
      proj.lastActivityAt = iso(now());
      return json(res, 200, { ok: true });
    }
    if (p === "/api/app/action" && body.action === "approve") {
      const proj = (freshProjects ?? projects).find((x) => x.workspaceId === body.workspace);
      const plan = proj?.plan;
      const waiting = (plan?.plans || []).filter((x) => x.awaitingApproval);
      const target = body.plan ? (plan?.plans || []).find((x) => x.id === body.plan) : waiting.length === 1 ? waiting[0] : null;
      if (!target) return json(res, 404, { error: waiting.length > 1 ? "several plans are waiting: name one" : "plan not found" });
      if (!target.awaitingApproval) return json(res, 409, { error: "plan is not waiting for approval" });
      target.awaitingApproval = false;
      plan.awaitingApproval = plan.plans.some((x) => x.awaitingApproval);
      for (const n of plan.nodes) if (n.status === "ready") n.status = "running";
      return json(res, 200, { ok: true, status: "delivered" });
    }
    if (p === "/api/app/action") {
      const t = transcriptFor(body.workspace, body.run);
      if (!t) return json(res, 404, { error: "Agent not found" });
      if (body.action === "delete") {
        const proj = projects.find((x) => x.agents.includes(t.a));
        if (proj) proj.agents = proj.agents.filter((x) => x !== t.a);
        return json(res, 200, { ok: true, status: "delivered" });
      }
      if (body.action === "pin" || body.action === "unpin") {
        if (body.action === "pin") t.a.pinned = true; else delete t.a.pinned;
        return json(res, 200, { ok: true, status: "delivered" });
      }
      if (body.action === "stop") { t.a.status = "stopped"; t.a.lastMessage = "Stopped by you."; }
      else if (body.action === "merge") {
        if (!["done", "review"].includes(t.a.status)) return json(res, 409, { error: "Only finished agents can be merged." });
        t.a.status = "merged"; t.a.lastMessage = "Merged into main.";
      } else return json(res, 400, { error: "Unknown action" });
      t.a.updatedAt = now();
      t.msgs.push({ id: `s${now()}`, role: "system", ts: now(), text: body.action === "stop" ? "Agent stopped from the app." : "Merged into main." });
      return json(res, 200, { ok: true });
    }
    if (p === "/api/app/agent-rename") {
      // Like the dashboard: a name with "fail" in it is refused, and the
      // state shows a new name a moment after the rename is delivered.
      const t = transcriptFor(body.workspace, body.run);
      if (!t) return json(res, 404, { error: "Agent not found" });
      const title = String(body.title || "").replace(/\s+/g, " ").trim();
      if (!title) return json(res, 400, { error: "a name can't be empty" });
      await delay(300);
      if (/fail/i.test(title)) return json(res, 502, { error: "the dashboard could not deliver it" });
      setTimeout(() => { t.a.title = title.slice(0, 200); }, 1500);
      return json(res, 200, { ok: true, status: "delivered" });
    }
    if (p === "/api/app/pin") {
      const proj = projects.find((x) => x.workspaceId === body.workspace);
      if (!proj) return json(res, 404, { error: "project not found" });
      proj.pinnedAt = body.pinned === true ? proj.pinnedAt || iso(now()) : null;
      return json(res, 200, { workspaceId: proj.workspaceId, pinnedAt: proj.pinnedAt });
    }
    if (p === "/api/app/project-order") {
      const ids = body.workspaces;
      if (!Array.isArray(ids) || new Set(ids).size !== ids.length) return json(res, 400, { error: "workspaces must list project ids, each once" });
      if (ids.some((id) => !projects.some((x) => x.workspaceId === id))) return json(res, 404, { error: "project not found" });
      for (const x of projects) x.sortOrder = ids.includes(x.workspaceId) ? ids.indexOf(x.workspaceId) : null;
      return json(res, 200, { workspaces: ids });
    }
    // Sign-ins: what logins.mjs on the machine answers. A machine that does
    // not exist yet is created first (503 until it is up, like the server).
    if (p === "/api/app/login/start") {
      if (homeMode !== "running") {
        if (homeMode !== "queued") { homeMode = "queued"; setTimeout(() => { homeMode = "running"; }, 5000); }
        return json(res, 503, { error: "the cloud machine is still starting; try again in a moment" });
      }
      const provider = body.provider;
      const id = `login-${now().toString(36)}`;
      const device = provider === "codex" && body.device === true;
      logins[provider] = { id, provider, mode: device ? "device" : "browser", status: "waiting", error: null, startedAt: iso(now()), endedAt: null };
      if (device) {
        setTimeout(() => {
          if (logins.codex?.id !== id || logins.codex.status !== "waiting") return;
          Object.assign(logins.codex, { status: "done", endedAt: iso(now()) });
          creds = { ...creds, codex: signedIn("codex") };
        }, 7000);
      }
      return json(res, 200, {
        ...logins[provider],
        url: device ? "https://auth.openai.com/codex/device" : provider === "claude"
          ? "https://claude.com/cai/oauth/authorize?code=true&redirect_uri=http%3A%2F%2Flocalhost%3A41643%2Fcallback&state=mock"
          : "https://auth.openai.com/oauth/authorize?redirect_uri=http%3A%2F%2F127.0.0.1%3A1455%2Fauth%2Fcallback&state=mock",
        callback: device ? null : { port: provider === "claude" ? 41643 : 1455, path: provider === "claude" ? "/callback" : "/auth/callback" },
        codeUrl: provider === "claude" ? "https://claude.com/cai/oauth/authorize?code=true&redirect_uri=https%3A%2F%2Fplatform.claude.com%2Foauth%2Fcode%2Fcallback&state=mock" : null,
        userCode: device ? "1QX5-T3MAL" : null,
      });
    }
    if (p === "/api/app/login/finish") {
      const login = logins[body.provider];
      if (!login || login.id !== body.id) return json(res, 400, { error: "That sign-in is no longer running. Start it again." });
      await delay(900);
      if (body.code === "bad") Object.assign(login, { status: "failed", error: "OAuth error: Invalid code", endedAt: iso(now()) });
      else {
        Object.assign(login, { status: "done", endedAt: iso(now()) });
        creds = { ...creds, [body.provider]: signedIn(body.provider) };
      }
      return json(res, 200, login);
    }
    if (p === "/api/app/login/cancel") {
      const login = logins[body.provider];
      if (login && login.status === "waiting") Object.assign(login, { status: "cancelled", endedAt: iso(now()) });
      return json(res, 200, { ok: true });
    }
    if (p === "/api/app/computer/lookup" || p === "/api/app/computer/approve") {
      const code = String(body.code || "").toUpperCase().replace(/[^A-Z0-9]/g, "").replace(/^(.{3})(.{3})$/, "$1-$2");
      const computer = computers.get(code);
      if (!computer || computer.expiresAt < now()) return json(res, 400, { error: "that code is wrong or expired; run `rudder login` again for a new one" });
      if (p.endsWith("/lookup")) return json(res, 200, { code, deviceName: computer.deviceName, expiresAt: iso(computer.expiresAt) });
      computers.delete(code);
      return json(res, 200, { ok: true, approved: body.approve === true, deviceName: computer.deviceName });
    }
    if (p === "/api/app/github/start") {
      githubFlow = githubFlow || { userCode: "WDJB-MJHT", polls: 0 };
      return json(res, 200, { status: "waiting", userCode: githubFlow.userCode, verificationUri: "https://github.com/login/device", expiresAt: iso(now() + 15 * MIN), interval: 5 });
    }
    if (p === "/api/app/github/poll") {
      if (!githubFlow) return json(res, 200, { status: "failed", error: "The code expired. Start again." });
      githubFlow.polls += 1;
      if (githubFlow.polls < 3) return json(res, 200, { status: "waiting", userCode: githubFlow.userCode, verificationUri: "https://github.com/login/device", interval: 5 });
      githubFlow = null;
      githubConnected = true;
      return json(res, 200, { status: "done", account: "viraatdas" });
    }
    if (p === "/api/app/github/cancel") {
      githubFlow = null;
      return json(res, 200, { ok: true });
    }
    if (p === "/api/app/wake") {
      homeMode = "queued";
      setTimeout(() => { homeMode = "running"; }, 8000);
      return json(res, 200, { home: home() });
    }
    if (p === "/api/app/screen") {
      return json(res, 200, { updatedAt: Date.now(), text: [
        " Select model",
        " Switch between Claude models. Applies to this session.",
        "",
        "   1. Default (recommended)  Opus 5.5 for complex work",
        " ❯ 2. Sonnet                  Sonnet 5.5 for daily coding",
        "   3. Haiku                   Haiku 5.5 for quick answers",
        "",
        " Enter to confirm · Esc to exit",
      ].join("\n") });
    }
    if (p === "/api/app/keys") {
      return json(res, 200, { ok: true, status: "delivered" });
    }
    if (p === "/api/app/models") {
      return json(res, 200, {
        current: { backend: "claude", model: "claude-opus-5-5" },
        claude: [{ label: "Default" }, { label: "Opus 5.5", value: "claude-opus-5-5", detail: "most capable" }, { label: "Sonnet 5.5", value: "claude-sonnet-5-5", detail: "fast and strong" }, { label: "Fable 5.1", value: "claude-fable-5-1", detail: "long tasks" }, { label: "Haiku 4.5", value: "claude-haiku-4-5", detail: "quickest" }],
        codex: [{ label: "Default" }, { label: "GPT-6.1 Sol", value: "gpt-6.1-sol", detail: "flagship" }, { label: "GPT-5.3 Codex", value: "gpt-5.3-codex", detail: "coding tuned" }],
      });
    }
    if (p === "/api/app/repos") {
      return json(res, 200, {
        owners: ["viraatdas", "exla-ai"],
        repos: [
          { repo: "exla-ai/smol", description: "Smol models", private: true, updatedAt: new Date(Date.now() - 3600e3).toISOString(), open: true },
          { repo: "exla-ai/vizzer", description: null, private: true, updatedAt: new Date(Date.now() - 86400e3).toISOString(), open: false },
          { repo: "exla-ai/rudder", description: "Run coding agents in parallel", private: false, updatedAt: new Date().toISOString(), open: true },
        ],
      });
    }
    if (p === "/api/app/create" && body.local) {
      const name = String(body.name || "");
      const ws = `wsl_${name.replace(/[^\w]/g, "_")}`;
      const list = freshProjects ?? projects;
      if (list.some((x) => x.name === name)) return json(res, 409, { error: `a project called ${name} is already on your cloud machine` });
      list.push({ workspaceId: ws, name, repo: "", ref: null, status: "queued", failureReason: null, dashboard: "cloning", clients: 0, lastActivityAt: iso(now()), agents: [], ports: [] });
      setTimeout(() => { const x = list.find((q) => q.workspaceId === ws); if (x) { x.dashboard = "running"; x.status = "running"; } }, 4000);
      if (homeMode === "none" || homeMode === "stopped") homeMode = "queued", setTimeout(() => { homeMode = "running"; }, 6000);
      return json(res, 200, { home: home(), workspace: { workspaceId: ws } });
    }
    if (p === "/api/app/create") {
      return json(res, 200, { workspace: { id: "ws-new", workspaceId: "ws-new", repo: `${body.owner}/${body.name}` } });
    }
    if (p === "/api/app/open") {
      const repo = String(body.repo || "");
      if (!/^[\w.-]+\/[\w.-]+$/.test(repo)) return json(res, 400, { error: "Repository must look like owner/name." });
      const ws = `wsp_${repo.replace(/[^\w]/g, "_")}`;
      const list = freshProjects ?? projects;
      if (!list.find((x) => x.workspaceId === ws)) {
        list.push({ workspaceId: ws, name: repo.split("/")[1], repo, ref: null, status: "queued", failureReason: null, dashboard: "cloning", clients: 0, lastActivityAt: iso(now()), agents: [], ports: [] });
        setTimeout(() => { const x = list.find((q) => q.workspaceId === ws); if (x) { x.dashboard = "running"; x.status = "running"; } }, 10000);
      }
      if (homeMode === "none" || homeMode === "stopped") homeMode = "queued", setTimeout(() => { homeMode = "running"; }, 6000);
      return json(res, 200, { home: home(), workspace: { workspaceId: ws } });
    }
    return json(res, 404, { error: "Not found" });
  }

  if (p === "/app" || p.startsWith("/app/")) return serveStatic(req, res, url);
  json(res, 404, { error: "Not found" });
});

server.listen(PORT, () => console.log(`Rudder app mock on http://localhost:${PORT}/app`));
