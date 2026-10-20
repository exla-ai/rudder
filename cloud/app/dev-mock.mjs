// Local development server for the Rudder phone app. No dependencies.
//   PORT=4321 node cloud/app/dev-mock.mjs
// Serves /app/* from this folder and mocks /api/app/*.
// Mock helpers: /app/__mock/signout (simulate signed out), /app/login (sign back in),
// /app/__mock/home/<running|stopped|failed|none> (machine states),
// /app/__mock/creds/<out|claude|both|expired> (Claude Code / Codex sign-in),
// /app/__mock/fresh (a brand new account: no GitHub, no agent login, no repos),
// /app/__mock/computer (prints a code to approve, as `rudder login` would),
// /app/__mock/onboard (show the setup picker again), /app/__mock/integrations/<on|off|unset>,
// /app/__mock/invite (prints a join link). Orgs, invites, Slack/Linear and a plan are mocked too,
// and so is the team view (two teammates in Exla, one asleep; uses cloud/dist/team.js).
// The digest uses the real worker/digest.mjs and cloud/dist/digest.js (build cloud first).
import http from "node:http";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { digestRun } from "../worker/digest.mjs";

const DIR = path.dirname(fileURLToPath(import.meta.url));
const PORT = Number(process.env.PORT || 4321);
const MIN = 60_000;
const now = () => Date.now();
const iso = (ms) => new Date(ms).toISOString();

let signedOut = process.env.MOCK_SIGNED_OUT === "1";
let homeMode = "running";
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

// MOCK_TRANSCRIPT=<file>: a real /api/app/transcript response (readTranscript
// output) shown as the first agent's conversation.
const realTranscript = process.env.MOCK_TRANSCRIPT ? JSON.parse(fs.readFileSync(process.env.MOCK_TRANSCRIPT, "utf8")) : null;
const transcripts = new Map();
function transcriptFor(ws, run) {
  const p = projects.find((x) => x.workspaceId === ws);
  const a = p?.agents.find((x) => x.id === run);
  if (!a) return null;
  if (!transcripts.has(run) && realTranscript && a === projects[0].agents[0]) transcripts.set(run, realTranscript.messages);
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

function serveStatic(req, res, url) {
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
    ? { slack: { connected: true, team: { id: "T1", name: "Exla" }, channel: { id: "C2", name: "agents" } }, linear: { connected: true, workspace: { name: "Exla" } } }
    : { slack: { connected: false, team: null, channel: null }, linear: { connected: false, workspace: null } });
  return integStore.get(scope);
}
const mine = (o) => o.members.find((m) => m.accountId === ME);
const orgList = () => [...orgs.values()].filter(mine).map((o) => ({ id: o.id, name: o.name, role: mine(o).role, memberCount: o.members.length, createdAt: o.createdAt }));
const ISSUES = [
  { id: "lin_1", identifier: "EXL-142", title: "Preview links should open in the phone app", url: "https://linear.app/exla/issue/EXL-142", state: "In Progress", priority: 2, team: "Rudder" },
  { id: "lin_2", identifier: "EXL-139", title: "Digest misses agents merged after midnight", url: "https://linear.app/exla/issue/EXL-139", state: "Todo", priority: 1, team: "Rudder" },
  { id: "lin_3", identifier: "EXL-120", title: "Settle bets when a market resolves", url: "https://linear.app/exla/issue/EXL-120", state: "Backlog", priority: 3, team: "Wager" },
];

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
  if (p.startsWith("/api/app/integrations") || p.startsWith("/api/app/linear/")) {
    if (!orgOk) return send(404, { error: "Organization not found" });
  }
  const configured = integMode !== "unset";
  if (p === "/api/app/integrations" && m === "GET") {
    const it = integ(scope);
    const on = integMode === "on";
    return send(200, {
      slack: { configured, connected: configured && on && it.slack.connected, team: on ? it.slack.team : null, channel: on ? it.slack.channel : null },
      linear: { configured, connected: configured && on && it.linear.connected, workspace: on ? it.linear.workspace : null },
      canManage,
    });
  }
  if ((r = p.match(/^\/api\/app\/integrations\/(slack|linear)\/start$/)) && m === "POST") {
    if (!configured) return send(503, { error: `${r[1] === "slack" ? "Slack" : "Linear"} is not set up on this server yet.` });
    if (!canManage) return send(403, { error: "Only admins can do that." });
    integMode = "on";
    return send(200, { url: `/app/__mock/oauth/${r[1]}${scope !== "personal" ? `?org=${encodeURIComponent(scope)}` : ""}` });
  }
  if ((r = p.match(/^\/api\/app\/integrations\/(slack|linear)$/)) && m === "DELETE") {
    if (!canManage) return send(403, { error: "Only admins can do that." });
    const it = integ(scope);
    if (r[1] === "slack") it.slack = { connected: false, team: null, channel: null }; else it.linear = { connected: false, workspace: null };
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
  if (p === "/api/app/linear/issues" && m === "GET") {
    if (!(integMode === "on" && integ(scope).linear.connected)) return send(409, { error: "Linear is not connected." });
    const q = (url.searchParams.get("q") || "").toLowerCase();
    return send(200, { issues: ISSUES.filter((i) => !q || `${i.identifier} ${i.title}`.toLowerCase().includes(q)) });
  }
  if (p === "/api/app/linear/start" && m === "POST") {
    const issue = ISSUES.find((i) => i.id === body.issueId);
    if (!issue) return send(404, { error: "Issue not found" });
    const list = freshProjects ?? projects;
    const proj = list.find((x) => x.workspaceId === body.workspace);
    const text = `${issue.identifier}: ${issue.title}\n\n${issue.url}`;
    if (proj) proj.agents.unshift(agent({ id: `run_l${Date.now() % 1e6}`, title: issue.title, task: text, status: "working", backend: "claude", model: "opus", createdAt: now(), updatedAt: now(), fresh: true, lastMessage: "Starting on the Linear issue." }));
    return send(200, { ok: true, text });
  }
  if ((r = p.match(/^\/api\/app\/link\/slack\/([^/]+)$/)) && m === "POST") {
    if (r[1] === "expired") return send(410, { error: "This link has expired. Run /rudder link in Slack again." });
    return send(200, { ok: true, slack: { team: "Exla", user: "viraat" } });
  }
  return undefined;
}

const delay = (ms) => new Promise((r) => setTimeout(r, ms));

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
    if (provider === "linear") it.linear = { ...it.linear, connected: true, workspace: { name: "Exla" } };
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
        github: { connected: githubConnected, source: githubConnected ? "sync" : null, updatedAt: null },
        digest: digestPreviewOf(digestStore[0]) ?? null,
        onboarding: { ...onboarding },
        orgs: orgList(),
      });
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
    if (p === "/api/app/transcript" && req.method === "GET") {
      const t = transcriptFor(url.searchParams.get("workspace"), url.searchParams.get("run"));
      if (!t) return json(res, 404, { error: "Agent not found" });
      const limit = Number(url.searchParams.get("limit") || 300);
      return json(res, 200, { runId: t.a.id, title: t.a.title, backend: t.a.backend, status: t.a.status, truncated: t.msgs.length > limit, messages: t.msgs.slice(-limit) });
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
      if (body.action === "stop") { t.a.status = "stopped"; t.a.lastMessage = "Stopped by you."; }
      else if (body.action === "merge") {
        if (!["done", "review"].includes(t.a.status)) return json(res, 409, { error: "Only finished agents can be merged." });
        t.a.status = "merged"; t.a.lastMessage = "Merged into main.";
      } else return json(res, 400, { error: "Unknown action" });
      t.a.updatedAt = now();
      t.msgs.push({ id: `s${now()}`, role: "system", ts: now(), text: body.action === "stop" ? "Agent stopped from the app." : "Merged into main." });
      return json(res, 200, { ok: true });
    }
    if (p === "/api/app/pin") {
      const proj = projects.find((x) => x.workspaceId === body.workspace);
      if (!proj) return json(res, 404, { error: "project not found" });
      proj.pinnedAt = body.pinned === true ? proj.pinnedAt || iso(now()) : null;
      return json(res, 200, { workspaceId: proj.workspaceId, pinnedAt: proj.pinnedAt });
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
