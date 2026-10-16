// Local development server for the Rudder phone app. No dependencies.
//   PORT=4321 node cloud/app/dev-mock.mjs
// Serves /app/* from this folder and mocks /api/app/*.
// Mock helpers: /app/__mock/signout (simulate signed out), /app/login (sign back in),
// /app/__mock/home/<running|stopped|failed|none> (machine states).
import http from "node:http";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const DIR = path.dirname(fileURLToPath(import.meta.url));
const PORT = Number(process.env.PORT || 4321);
const MIN = 60_000;
const now = () => Date.now();
const iso = (ms) => new Date(ms).toISOString();

let signedOut = process.env.MOCK_SIGNED_OUT === "1";
let homeMode = "running";
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
});

const agent = (o) => ({
  phase: null, model: null, mode: "execute", lastMessage: null, summary: null, nodeId: null, planId: null, remoteUrl: null,
  createdAt: T0 - 90 * MIN, updatedAt: T0 - 10 * MIN, ...o,
});

const projects = [
  {
    workspaceId: "wsp_rudder", name: "rudder", repo: "viraatdas/rudder", ref: "main", status: "running", failureReason: null,
    dashboard: "running", clients: 1, lastActivityAt: iso(T0 - 1 * MIN),
    agents: [
      agent({ id: "run_a1", title: "Phone app for agent control", task: "Build the phone-first web app under cloud/app", status: "working", backend: "claude", model: "opus", mode: "main", updatedAt: T0 - 1 * MIN, lastMessage: "Wiring the transcript poll to keep scroll pinned only when you are already at the bottom.", remoteUrl: "https://claude.ai/code/session_01Abc" }),
      agent({ id: "run_a2", title: "Fix flaky Ctrl+W chord test", task: "Stabilize the e2e chord tests", status: "waiting", backend: "codex", model: "gpt-6.1-sol", phase: "permission", updatedAt: T0 - 4 * MIN, lastMessage: "I need permission to run `cargo test --release` with network access. Allow?" }),
      agent({ id: "run_a3", title: "Tailnet HTTPS for previews", task: "Expose dev servers over tailnet https", status: "review", backend: "claude", model: "sonnet", nodeId: "n2", planId: "plan_1", updatedAt: T0 - 18 * MIN, lastMessage: "Done. Ports 3000 and 5173 are reachable at https://rudder-home.tail4c2e1.ts.net. Ready to merge." }),
      agent({ id: "run_a4", title: "Delete cloud workspaces safely", task: "Request + reaper for deletes", status: "done", backend: "claude", model: "opus", updatedAt: T0 - 3 * 60 * MIN, lastMessage: "Added the deletion reaper with a 30s sweep and 410s for dead rows." }),
      agent({ id: "run_a5", title: "Bump jj to 0.40 flags", task: "jj rebase -o", status: "merged", backend: "codex", model: "gpt-6.1-sol", updatedAt: T0 - 26 * 60 * MIN, lastMessage: "Merged into main." }),
      agent({ id: "run_a6", title: "Explore opencode resume", task: "opencode session resume", status: "stopped", backend: "opencode", model: "anthropic/claude-sonnet", updatedAt: T0 - 50 * 60 * MIN, lastMessage: "Stopped by you." }),
    ],
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
    { id: id(), role: "tool", ts: t + 90_000, text: "", tool: { name: "Bash", input: "npm run check", output: "> @viraatdas/rudder@2.15.4 check\n> tsc --noEmit\n\nsrc/cloud.ts(418,7): error TS2322: Type 'string | undefined' is not assignable to type 'string'.\n  Type 'undefined' is not assignable to type 'string'.", isError: true } },
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

const transcripts = new Map();
function transcriptFor(ws, run) {
  const p = projects.find((x) => x.workspaceId === ws);
  const a = p?.agents.find((x) => x.id === run);
  if (!a) return null;
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

const delay = (ms) => new Promise((r) => setTimeout(r, ms));

const server = http.createServer(async (req, res) => {
  const url = new URL(req.url, `http://${req.headers.host}`);
  const p = url.pathname;

  if (p === "/") { res.writeHead(302, { Location: "/app" }); return res.end(); }
  if (p.startsWith("/app/login")) { signedOut = false; res.writeHead(302, { Location: "/app" }); return res.end(); }
  if (p === "/app/__mock/signout") { signedOut = true; res.writeHead(302, { Location: "/app" }); return res.end(); }
  if (p.startsWith("/app/__mock/home/")) { homeMode = p.split("/").pop(); res.writeHead(302, { Location: "/app" }); return res.end(); }
  if (p === "/api/auth/sign-out" && req.method === "POST") { signedOut = true; return json(res, 200, { success: true }); }

  if (p.startsWith("/api/app/")) {
    await delay(120 + Math.random() * 180);
    if (signedOut) return json(res, 401, { error: "Not signed in" });

    if (p === "/api/app/state" && req.method === "GET") {
      // Living data: the working agent's message ticks forward.
      const a1 = projects[0].agents[0];
      a1.updatedAt = now() - 20_000;
      return json(res, 200, { email: "viraat@example.com", home: home(), projects: homeMode === "none" ? [] : projects });
    }
    if (p === "/api/app/transcript" && req.method === "GET") {
      const t = transcriptFor(url.searchParams.get("workspace"), url.searchParams.get("run"));
      if (!t) return json(res, 404, { error: "Agent not found" });
      const limit = Number(url.searchParams.get("limit") || 300);
      return json(res, 200, { runId: t.a.id, title: t.a.title, backend: t.a.backend, status: t.a.status, truncated: t.msgs.length > limit, messages: t.msgs.slice(-limit) });
    }
    const body = req.method === "POST" ? await readBody(req) : {};
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
        claude: [{ label: "Default" }, { label: "Opus 5.5", value: "claude-opus-5-5" }, { label: "Sonnet 5.5", value: "claude-sonnet-5-5" }, { label: "Fable 5.1", value: "claude-fable-5-1" }],
        codex: [{ label: "Default" }, { label: "GPT-6.1 Sol", value: "gpt-6.1-sol" }, { label: "GPT-5.3 Codex", value: "gpt-5.3-codex" }],
      });
    }
    if (p === "/api/app/repos") {
      return json(res, 200, {
        owners: ["viraatdas", "exla-ai"],
        repos: [
          { repo: "exla-ai/smol", description: "Smol models", private: true, updatedAt: new Date(Date.now() - 3600e3).toISOString(), open: true },
          { repo: "exla-ai/vizzer", description: null, private: true, updatedAt: new Date(Date.now() - 86400e3).toISOString(), open: false },
          { repo: "viraatdas/rudder", description: "Run coding agents in parallel", private: false, updatedAt: new Date().toISOString(), open: true },
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
      if (!projects.find((x) => x.workspaceId === ws)) {
        projects.push({ workspaceId: ws, name: repo.split("/")[1], repo, ref: null, status: "queued", failureReason: null, dashboard: "cloning", clients: 0, lastActivityAt: iso(now()), agents: [], ports: [] });
        setTimeout(() => { const x = projects.find((q) => q.workspaceId === ws); if (x) { x.dashboard = "running"; x.status = "running"; } }, 10000);
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
