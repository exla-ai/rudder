import { spawn } from "node:child_process";
import { createHash, randomUUID } from "node:crypto";
import { AttachSession, containsCtrlC } from "./attach-session.js";
import { PasteBridge, readLocalClipboard } from "./paste-bridge.js";
import fsp from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { WebSocket } from "ws";
import { currentBranch, currentCommit, findRepoRoot } from "./git.js";
import {
  type MigrationCandidate,
  type MigrationManifestEntry,
  type MigrationPlan,
  type MigrationSnapshotManifest,
  applyDefaultDecisions,
  buildFreshHandoffPrompt,
  cloudWorkspaceAbsolutePath,
  findMigrationCandidates,
  formatCandidateReason,
  migrationSummary,
  summaryAsJson,
} from "./migration.js";
import { buildMoveBundle, findMoveCandidates, markRunsMovedHome, type MoveCandidate } from "./move-home.js";
import { cloudAuthPath } from "./state.js";
import { runLaptopOnboarding, type CloudRepoSummary } from "./onboarding.js";
import { renderQrForTerminal } from "./qr.js";
import { describeSync, exportWork, importWork, syncCheckout, type CheckoutSyncReport } from "./checkout-sync.js";
import { pullFromHome } from "./cloud-pull.js";
import type { CloudAuthState, CloudSail, JsonValue } from "./types.js";
import {
  ensureDir,
  commandExists,
  expandHome,
  isTty,
  newRunId,
  nowIso,
  pathExists,
  promptConfirm,
  promptText,
  promptSelect,
  promptSecret,
  readJson,
  runCommand,
  shortenHome,
  shellQuote,
  writeJson,
} from "./util.js";

type CloudCommandOptions = {
  json?: boolean;
  homePaths?: string[];
  sshHost?: string;
  noAttach?: boolean;
  quietBanner?: boolean;
  latencyProbe?: boolean;
  /** Coming back to a repo already open on the home machine: skip the first-run offers. */
  reattach?: boolean;
  /** Skip the y/N confirmation of a destructive command (`--yes`/`--force`). */
  yes?: boolean;
};

type LoginStartResponse = {
  loginUrl?: string;
  verificationUri?: string;
  pollUrl?: string;
  deviceCode?: string;
  /** The short code a signed-in phone approves, and the page it does it on. */
  userCode?: string;
  approveUrl?: string;
  interval?: number;
  expiresIn?: number;
};

type LoginPollResponse = {
  pending?: boolean;
  denied?: boolean;
  /** "phone" when a signed-in phone approved this login. */
  via?: string;
  token?: string;
  accessToken?: string;
  accountId?: string;
  email?: string;
  expiresAt?: string;
  expiresIn?: number;
};

type SnapshotManifest = {
  version: 1;
  createdAt: string;
  repo: {
    root: string;
    branch: string;
    commit: string;
  };
  homePaths: string[];
  rudderState?: {
    runs: number;
    files: string[];
  };
  migratedAgents?: number;
  capturedEnvVars?: number;
  projectEnvFiles?: number;
};

type SnapshotOptions = {
  includeRudderState?: boolean;
  // Ship only these runs' records instead of all of .rudder/runs. A single-run
  // handoff must not carry the rest of the fleet: records marked "running"
  // would be resumed by the cloud dashboard while still running locally.
  rudderStateRunIds?: string[];
  // False once the account has vault secrets: the snapshot then carries only
  // the working tree, never home-dir credentials or captured env vars.
  includeCredentials?: boolean;
  migration?: {
    repoName: string;
    plan: MigrationPlan;
  };
};

type CloudClient = {
  baseUrl: string;
  request<T>(pathOrUrl: string, init: { method: string; body?: JsonValue }): Promise<T>;
  download(pathname: string): Promise<Buffer>;
};

type CloudRuntime = "fly" | "byo-vm";

const DEFAULT_LOGIN_INTERVAL_MS = 2000;
const DEFAULT_LOGIN_TIMEOUT_MS = 5 * 60 * 1000;
const DEFAULT_CLOUD_URL = "https://rudder-cloud-control.fly.dev";
const GITHUB_CLI_CLIENT_ID = "178c6fc778ccc68e1d6a";
const MAX_HOME_SECRET_SCAN_BYTES = 1024 * 1024;
const DEFAULT_HOME_PATHS = [
  // Agent CLIs
  "~/.claude/.credentials.json",
  "~/.claude/settings.json",
  // Hook scripts settings.json points at; the home machine rewrites their
  // /Users/<name> paths to its own home (cloud/worker/hook-paths.mjs).
  "~/.claude/hooks",
  "~/.claude/CLAUDE.md",
  "~/.claude.json",
  "~/.codex/auth.json",
  "~/.codex/config.toml",
  "~/.codex/AGENTS.md",
  "~/.codex/hooks.json",
  "~/.codex/rules",
  // Git + GitHub
  "~/.gitconfig",
  "~/.config/gh",
  // Shell rc so PATH/aliases/env exports come along
  "~/.zshrc",
  "~/.zprofile",
  "~/.bashrc",
  "~/.bash_profile",
  "~/.profile",
  "~/.envrc",
  // Package managers
  "~/.npmrc",
  "~/.yarnrc",
  "~/.yarnrc.yml",
  "~/.cargo/config",
  "~/.cargo/config.toml",
  // Cloud provider CLIs
  "~/.vercel",
  "~/.config/vercel",
  "~/.aws/config",
  "~/.aws/credentials",
  "~/.config/gcloud/configurations",
  "~/.config/gcloud/active_config",
  "~/.config/gcloud/credentials.db",
  "~/.config/gcloud/access_tokens.db",
  "~/.config/gcloud/application_default_credentials.json",
  "~/.kube/config",
  // Netrc for tools that auth via netrc
  "~/.netrc",
];
// Paths or path components that should never be uploaded under any
// circumstance — even if a user's DEFAULT_HOME_PATHS entry references them.
// Specifically leaving out .aws/.kube/.docker so the corresponding configs
// can ship; .ssh/.gnupg/keychains stay blocked because they contain
// private key material that isn't recoverable if leaked.
const SECRET_PATH_PARTS = new Set([
  ".ssh",
  ".gnupg",
  "keychains",
]);
const BULKY_HOME_PATH_PARTS = new Set([
  "archived_sessions",
  "backups",
  "cache",
  "file-history",
  "log",
  "paste-cache",
  "plugins",
  "projects",
  "session",
  "sessions",
  "shell_snapshots",
  "skills",
  "telemetry",
  "todos",
  "workspaces",
]);
const SECRET_BASENAMES = new Set([
  ".env",
  ".env.local",
  ".env.production",
  ".env.development",
  "id_rsa",
  "id_ed25519",
  "known_hosts",
]);
const BULKY_HOME_BASENAME_PATTERNS = [
  /^history\./,
  /^logs?_/,
  /^state_\d+\.sqlite/,
  /\.sqlite(?:-(?:wal|shm))?$/,
  /\.log$/,
  /\.jsonl$/,
];

export async function runCloudCommand(command: string, args: string[], options: CloudCommandOptions = {}): Promise<void> {
  // Hidden diagnostic flag: measure keystroke round-trip latency instead of
  // starting an interactive attach. Parsed here because main.ts forwards
  // unknown flags through as positional args.
  if (args.includes("--latency-probe")) {
    args = args.filter((arg) => arg !== "--latency-probe");
    options = { ...options, latencyProbe: true };
  }
  const subcommand = args[0] ?? "";
  const rest = args.slice(1);

  if (command === "cloud" && subcommand === "help") {
    printCloudHelp();
    return;
  }

  // `rudder cloud` with no further args means "open the cloud workspace for
  // this repo". Explicit subcommands (sail, launch, etc.) keep their old
  // behavior. `rudder sail` and `rudder cloud sail` still launch a sail.
  if (command === "cloud" && (subcommand === "" || subcommand === "--snapshot")) {
    await openOnHome(subcommand === "--snapshot", options);
    return;
  }

  switch (subcommand) {
    case "login":
      await login(options, rest);
      return;
    case "home":
      await homeCommand(rest, options);
      return;
    case "doctor":
      await cloudDoctor(options);
      return;
    case "sync":
      await cloudSync(rest, options);
      return;
    case "app":
    case "pair":
      await printAppLink(options);
      return;
    case "launch":
      await launch(rest, options, "task");
      return;
    case "sail":
      await launch(rest, options);
      return;
    case "byoc":
      await setupByoc(rest, options);
      return;
    case "vm":
    case "byo-vm":
      await launch(rest, options, "task", "byo-vm");
      return;
    case "list":
    case "ls":
      await listSails(options);
      return;
    case "status":
      await status(options);
      return;
    case "logs":
      await logs(rest, options);
      return;
    case "attach":
      await attach(rest, options);
      return;
    case "talk":
    case "say":
    case "msg":
      await talk(rest, options);
      return;
    case "output":
    case "tail":
      await tailOutput(rest, options);
      return;
    case "quickstart":
      await quickstart(options);
      return;
    case "slack":
      await slackSetup(rest, options);
      return;
    case "workspace":
      await workspaceCommand(rest, options);
      return;
    case "onload":
      await onload(rest, options);
      return;
    case "move":
      await moveCommand(rest, options);
      return;
    case "pull":
      await pullCommand(rest, options);
      return;
    case "bootstrap":
      await bootstrap(rest, options);
      return;
    case "pause":
      await mutateSail("pause", rest, options);
      return;
    case "resume":
      await mutateSail("resume", rest, options);
      return;
    case "stop":
      await mutateSail("stop", rest, options);
      return;
    case "delete":
    case "rm":
      await deleteSail(rest, options);
      return;
    case "setup-github":
      await setupOAuthProvider("github", rest, options);
      return;
    case "setup-google":
      await setupOAuthProvider("google", rest, options);
      return;
    case "setup-byoc":
      await setupByoc(rest, options);
      return;
    case "setup-vm":
      await setupByoc(rest, options);
      return;
    case "setup-fly":
      await configureDefaultRuntime("fly", options);
      return;
    case "setup":
      if (rest[0] === "byoc" || rest[0] === "vm" || rest[0] === "byo-vm") {
        await setupByoc(rest.slice(1), options);
        return;
      }
      if (rest[0] === "fly") {
        await configureDefaultRuntime("fly", options);
        return;
      }
      throw new Error("Usage: rudder cloud setup byoc | rudder cloud setup fly");
    case "runtime":
      await runtime(rest, options);
      return;
    case "region":
      await configureRegion(rest, options);
      return;
    case "secrets":
      await secretsCommand(rest, options);
      return;
    default:
      // A bare `rudder cloud "<text>"` / `rudder sail "<text>"` is the documented
      // way to launch a worker ON that task (the instance name is derived from it).
      // Use task mode so the worker actually RUNS the task instead of opening an
      // idle dashboard. Explicit `rudder cloud sail <name>` keeps name-only mode.
      // Except one word naming a project on the home machine (`rudder cloud
      // tempo`): that is "open tempo", not a task called "tempo".
      if (command === "cloud" && rest.length === 0 && /^[\w.\/-]+$/.test(subcommand) && await attachHomeProject(subcommand, options)) {
        return;
      }
      await launch(command === "sail" ? args : [subcommand, ...rest], options, "task");
      return;
  }
}

async function login(options: CloudCommandOptions, args: string[] = []): Promise<void> {
  const client = await cloudClient({ requireToken: false });
  // Google is the default sign-in: the browser opens straight on Google's
  // account picker instead of a page of buttons. `--github` picks GitHub.
  const provider = args.includes("--github") ? "github" : "google";
  const browserLogin = await tryBrowserLogin(client, options, provider).catch((error) => {
    // A no on the phone is an answer: never sign in some other way behind it.
    if (error instanceof LoginDeniedError) {
      throw error;
    }
    if (!options.json) {
      console.warn(`Browser login unavailable: ${error instanceof Error ? error.message : String(error)}`);
      console.warn("Trying local GitHub auth fallback...");
    }
    return null;
  });
  if (browserLogin?.token || browserLogin?.accessToken) {
    const token = browserLogin.token ?? browserLogin.accessToken;
    if (token) {
      await saveCloudLogin(client, browserLogin, token, options, browserLogin.via ?? "browser");
      return;
    }
  }

  const githubLogin = await tryGithubCliLogin(client).catch(() => null);
  if (githubLogin?.token || githubLogin?.accessToken) {
    const token = githubLogin.token ?? githubLogin.accessToken;
    if (token) {
      await saveCloudLogin(client, githubLogin, token, options, "GitHub CLI");
      return;
    }
  }

  const githubDeviceLogin = await tryGithubDeviceLogin(client, options).catch(() => null);
  if (githubDeviceLogin?.token || githubDeviceLogin?.accessToken) {
    const token = githubDeviceLogin.token ?? githubDeviceLogin.accessToken;
    if (token) {
      await saveCloudLogin(client, githubDeviceLogin, token, options, "GitHub device");
      return;
    }
  }
}

async function tryBrowserLogin(
  client: CloudClient,
  options: CloudCommandOptions,
  provider: "google" | "github" = "google",
): Promise<(LoginPollResponse & { via?: string }) | null> {
  const response = await client.request<LoginStartResponse>("/api/cli/login", {
    method: "POST",
    body: {
      deviceName: os.hostname(),
      client: "rudder",
    },
  });
  const deviceCode = response.deviceCode;
  const loginUrl = deviceCode
    ? withQuery(client.baseUrl, `/cli/oauth/${provider}/start`, { device_code: deviceCode })
    : response.loginUrl ?? response.verificationUri ?? withQuery(client.baseUrl, "/cli/login", {});
  const pollPath = response.pollUrl ?? "/api/cli/login/poll";
  const intervalMs = Math.max(1000, (response.interval ?? DEFAULT_LOGIN_INTERVAL_MS / 1000) * 1000);
  const timeoutMs = Math.max(intervalMs, (response.expiresIn ?? DEFAULT_LOGIN_TIMEOUT_MS / 1000) * 1000);

  // A phone already signed in to Rudder can approve this computer: show the
  // code and a QR of the approve page, and open the browser only on Enter.
  // Without a terminal to press Enter in, the browser opens as before.
  const interactive = !options.json && Boolean(process.stdin.isTTY && process.stdout.isTTY);
  let browserOpened = false;
  const openLoginBrowser = () => {
    if (browserOpened) {
      return;
    }
    browserOpened = true;
    console.log(`Opening ${loginUrl}`);
    if (!options.json) {
      openBrowser(loginUrl);
    }
    console.log("Waiting for browser login to complete...");
  };
  let enterListener: import("node:readline").Interface | null = null;
  if (response.userCode && response.approveUrl && !options.json) {
    console.log("");
    console.log("Sign this computer in from your phone: open Rudder, tap \"Sign in a computer\" and enter");
    console.log("");
    console.log(`    ${response.userCode}`);
    console.log("");
    if (process.stdout.isTTY) {
      try {
        console.log("or scan this with the phone's camera:");
        console.log(renderQrForTerminal(response.approveUrl, { indent: "    " }));
      } catch {
        // A url too long for the encoder: the code above still works.
      }
    }
    console.log(`(${response.approveUrl})`);
    console.log("");
    if (interactive) {
      console.log("Press Enter to sign in in the browser instead.");
      const readline = await import("node:readline");
      enterListener = readline.createInterface({ input: process.stdin });
      enterListener.on("line", openLoginBrowser);
    } else {
      openLoginBrowser();
    }
  } else {
    openLoginBrowser();
  }

  try {
    const startedAt = Date.now();
    while (Date.now() - startedAt < timeoutMs) {
      await sleep(intervalMs);
      const poll = await pollLogin(client, pollPath, deviceCode);
      const token = poll.token ?? poll.accessToken;
      if (token) {
        return { ...poll, via: poll.via === "phone" ? "your phone" : "browser" };
      }
      if (poll.denied) {
        throw new LoginDeniedError();
      }
      if (poll.pending === false) {
        throw new Error("Cloud login was not approved.");
      }
    }
    throw new Error("Timed out waiting for cloud login.");
  } finally {
    enterListener?.close();
  }
}

class LoginDeniedError extends Error {
  constructor() {
    super("The sign-in was denied on your phone. Run `rudder login` again to retry.");
  }
}

async function tryGithubCliLogin(client: CloudClient): Promise<LoginPollResponse | null> {
  if (process.env.RUDDER_SKIP_GH_CLI === "1") {
    return null;
  }
  const gh = await runCommand("gh", ["auth", "token"], { allowFailure: true });
  const token = gh.stdout.trim();
  if (gh.code !== 0 || !token) {
    return null;
  }
  return await client.request<LoginPollResponse>("/api/cli/login/github-token", {
    method: "POST",
    body: { token },
  });
}

async function tryGithubDeviceLogin(client: CloudClient, options: CloudCommandOptions): Promise<LoginPollResponse | null> {
  const start = await githubOAuthRequest<{
    device_code?: string;
    user_code?: string;
    verification_uri?: string;
    verification_uri_complete?: string;
    expires_in?: number;
    interval?: number;
    error?: string;
    error_description?: string;
  }>("https://github.com/login/device/code", {
    client_id: GITHUB_CLI_CLIENT_ID,
    scope: "read:user user:email",
  });
  if (!start.device_code || !start.user_code || !start.verification_uri) {
    return null;
  }
  if (!options.json) {
    const url = start.verification_uri_complete ?? start.verification_uri;
    console.log(`Opening ${url}`);
    console.log(`GitHub code: ${start.user_code}`);
    openBrowser(url);
  }

  const intervalMs = Math.max(1000, (start.interval ?? 5) * 1000);
  const timeoutMs = Math.max(intervalMs, (start.expires_in ?? 900) * 1000);
  const startedAt = Date.now();
  while (Date.now() - startedAt < timeoutMs) {
    await sleep(intervalMs);
    const poll = await githubOAuthRequest<{
      access_token?: string;
      token_type?: string;
      scope?: string;
      error?: string;
      error_description?: string;
      interval?: number;
    }>("https://github.com/login/oauth/access_token", {
      client_id: GITHUB_CLI_CLIENT_ID,
      device_code: start.device_code,
      grant_type: "urn:ietf:params:oauth:grant-type:device_code",
    });
    if (poll.access_token) {
      return await client.request<LoginPollResponse>("/api/cli/login/github-token", {
        method: "POST",
        body: { token: poll.access_token },
      });
    }
    if (poll.error === "authorization_pending") {
      continue;
    }
    if (poll.error === "slow_down") {
      await sleep(Math.max(intervalMs, (poll.interval ?? 5) * 1000));
      continue;
    }
    return null;
  }
  return null;
}

async function githubOAuthRequest<T>(url: string, body: Record<string, string>): Promise<T> {
  const response = await fetch(url, {
    method: "POST",
    headers: {
      Accept: "application/json",
      "Content-Type": "application/json",
    },
    body: JSON.stringify(body),
  });
  const text = await response.text();
  const parsed = text ? parseJson(text) : null;
  if (!response.ok) {
    throw new Error(responseErrorMessage(parsed) || text.trim() || `${response.status} ${response.statusText}`);
  }
  return parsed as T;
}

async function saveCloudLogin(
  client: CloudClient,
  login: LoginPollResponse,
  token: string,
  options: CloudCommandOptions,
  source: string,
): Promise<void> {
  const previous = await loadCloudAuth();
  const previousRuntime = previous?.cloudUrl === client.baseUrl ? parseCloudRuntime(previous.defaultRuntime) : undefined;
  const previousByocHost = previous?.cloudUrl === client.baseUrl ? previous.byocSshHost : undefined;
  await saveCloudAuth({
    version: 1,
    token,
    cloudUrl: client.baseUrl,
    defaultRuntime: previousRuntime,
    byocSshHost: previousByocHost,
    accountId: login.accountId,
    email: login.email,
    expiresAt: login.expiresAt ?? (login.expiresIn ? new Date(Date.now() + login.expiresIn * 1000).toISOString() : undefined),
    updatedAt: nowIso(),
  });
  if (options.json) {
    const result: Record<string, JsonValue> = { ok: true, cloudUrl: client.baseUrl, source };
    if (login.email) {
      result.email = login.email;
    }
    if (login.accountId) {
      result.accountId = login.accountId;
    }
    printJson(result);
  } else {
    console.log(`Logged in to ${client.baseUrl}${login.email ? ` as ${login.email}` : ""} via ${source}.`);
    // First run on this computer: agent CLIs, then what is in the cloud.
    await runLaptopOnboarding({ json: options.json, cloudRepos: cloudRepoSummaries });
  }
}

/** The repos open on this account's cloud machine, for onboarding; null when signed out or unreachable. */
export async function cloudRepoSummaries(): Promise<CloudRepoSummary[] | null> {
  try {
    const client = await cloudClient({ requireToken: true });
    const view = await client.request<{ projects?: { repo?: string; agents?: unknown[] }[] }>("/api/rudder/home", { method: "GET" });
    return (view.projects ?? [])
      .filter((project) => typeof project.repo === "string" && project.repo)
      .map((project) => ({ repo: String(project.repo), agents: Array.isArray(project.agents) ? project.agents.length : 0 }));
  } catch {
    return null;
  }
}

async function launch(
  args: string[],
  options: CloudCommandOptions,
  mode: "name" | "task" = "name",
  explicitRuntime?: CloudRuntime,
): Promise<void> {
  const raw = args.join(" ").trim();
  const repoRoot = findRepoRoot();
  const snapshot = await createSnapshot(repoRoot, options.homePaths ?? []);
  try {
    const client = await cloudClient({ requireToken: true });
    const runtime = await selectedCloudRuntime(explicitRuntime);
    const task = mode === "task" || runtime === "byo-vm" ? raw : "";
    const launchName = process.env.RUDDER_CLOUD_LAUNCH_NAME?.trim();
    const name = launchName || (task ? cloudNameFromTask(task) : raw || randomCloudName());
    const body: Record<string, JsonValue> = {
      repoName: path.basename(repoRoot),
      name,
      snapshot: {
        name: path.basename(snapshot.archivePath),
        contentType: "application/gzip",
        base64: await fsp.readFile(snapshot.archivePath, "base64"),
        manifest: snapshot.manifest as unknown as JsonValue,
      },
    };
    if (runtime !== "fly") {
      body.runtime = runtime;
    }
    if (task) {
      body.task = task;
    }
    const result = await client.request<JsonValue>("/api/rudder/sail/launch", {
      method: "POST",
      body,
    });
    await printResult(result, options);
    await maybeAutoAttach(result, options);
  } finally {
    await fsp.rm(snapshot.tempDir, { recursive: true, force: true });
  }
}

async function onload(args: string[], options: CloudCommandOptions): Promise<void> {
  const runId = args[0];
  const repoRoot = findRepoRoot();
  if (runId) {
    await handoffRunToCloud(repoRoot, runId, options);
    return;
  }
  const snapshot = await createSnapshot(repoRoot, options.homePaths ?? [], { includeRudderState: true });
  try {
    const client = await cloudClient({ requireToken: true });
    const runtime = await selectedCloudRuntime();
    const body: Record<string, JsonValue> = {
      repoName: path.basename(repoRoot),
      workspace: true,
      name: `workspace-${path.basename(repoRoot)}`,
      snapshot: {
        name: path.basename(snapshot.archivePath),
        contentType: "application/gzip",
        base64: await fsp.readFile(snapshot.archivePath, "base64"),
        manifest: snapshot.manifest as unknown as JsonValue,
      },
    };
    if (runtime !== "fly") {
      body.runtime = runtime;
    }
    const result = await client.request<JsonValue>("/api/rudder/sail/onload", {
      method: "POST",
      body,
    });
    await printResult(result, options);
    await maybeAutoAttach(result, options);
  } finally {
    await fsp.rm(snapshot.tempDir, { recursive: true, force: true });
  }
}

/**
 * Scope a fleet migration plan down to one run: only `runId` moves, every
 * other candidate stays local. Fleet migration classifies EVERY non-terminal
 * run with a workspace as movable, so without this a single-run handoff would
 * drag the rest of the fleet's records into the snapshot marked "migrate" —
 * and the cloud dashboard would resume agents that are still running locally.
 */
export function buildSingleRunPlan(candidates: MigrationCandidate[], runId: string): MigrationPlan {
  return applyDefaultDecisions(
    candidates.map((candidate) =>
      candidate.runId === runId ? candidate : { ...candidate, decision: "stay" as const },
    ),
  );
}

/**
 * `rudder cloud onload <runId>` — hand ONE local agent off to its own cloud
 * machine. The snapshot carries the repo, that run's record (and only that
 * run's), its isolated workspace, and its Claude session jsonl. The worker
 * boots the bare dashboard (no task ⇒ no fresh worker), which finds the
 * migration marker and resumes the conversation via `claude --resume` — the
 * same machinery as fleet migration, scoped to a single run.
 */
async function handoffRunToCloud(repoRoot: string, runId: string, options: CloudCommandOptions): Promise<void> {
  const candidates = await findMigrationCandidates(repoRoot);
  const candidate = candidates.find((entry) => entry.runId === runId);
  if (!candidate) {
    throw new Error(
      `Run ${runId} has no migratable record under .rudder/runs — it needs a non-terminal status and an isolated workspace.`,
    );
  }
  if (candidate.decision !== "migrate" && candidate.decision !== "migrate-fresh") {
    throw new Error(`Run ${runId} cannot be handed off: ${formatCandidateReason(candidate)}.`);
  }
  const plan = buildSingleRunPlan(candidates, runId);
  const repoName = path.basename(repoRoot);
  const client = await cloudClient({ requireToken: true });
  const vaultActive = await accountHasVaultSecrets(client);
  if (!options.json) {
    const how = candidate.decision === "migrate"
      ? `resuming session ${candidate.sessionId}`
      : `fresh restart with a context handoff (${formatCandidateReason(candidate)})`;
    process.stderr.write(`Handing off ${runId} to Rudder Cloud — ${how}...\n`);
  }
  const snapshot = await createSnapshot(repoRoot, options.homePaths ?? [], {
    includeCredentials: !vaultActive,
    migration: { repoName, plan },
    rudderStateRunIds: [runId],
  });
  try {
    const runtime = await selectedCloudRuntime();
    const body: Record<string, JsonValue> = {
      repoName,
      runId,
      snapshot: {
        name: path.basename(snapshot.archivePath),
        contentType: "application/gzip",
        base64: await fsp.readFile(snapshot.archivePath, "base64"),
        manifest: snapshot.manifest as unknown as JsonValue,
      },
    };
    if (runtime !== "fly") {
      body.runtime = runtime;
    }
    const result = await client.request<JsonValue>("/api/rudder/sail/onload", {
      method: "POST",
      body,
    });
    await printResult(result, options);
    // The sail owns the conversation now. The dashboard watches this record
    // for exactly that ("migrated") instead of assuming it at spawn time.
    await markRunsMigrated(repoRoot, [runId]);
    await maybeAutoAttach(result, options);
    // The handoff SUCCEEDED the moment the sail launched: the cloud machine
    // owns the conversation now. An attach that drops afterwards (network
    // blip, remote quit, machine still staging) must not fail this process —
    // the dashboard reads a non-zero exit as "handoff failed" and resumes the
    // agent LOCALLY, which would run a second copy of a conversation that is
    // alive in the cloud.
    process.exitCode = 0;
    if (!options.json) {
      process.stderr.write(`Handed off. Reattach any time: rudder cloud attach ${runId}\n`);
    }
  } finally {
    await fsp.rm(snapshot.tempDir, { recursive: true, force: true });
  }
}

async function logs(args: string[], options: CloudCommandOptions): Promise<void> {
  const workerId = args[0];
  if (!workerId) {
    throw new Error("Usage: rudder cloud logs <id>");
  }
  const client = await cloudClient({ requireToken: true });
  const [sailResult, workspaceResult] = await Promise.all([
    client.request<JsonValue>("/api/rudder/sail", { method: "GET" }),
    client.request<JsonValue>("/api/rudder/workspace", { method: "GET" }),
  ]);
  const sails = collectionFromResult(sailResult, "sails");
  const workspaces = collectionFromResult(workspaceResult, "workspaces");
  const match = [...sails, ...workspaces].find((item) =>
    item && typeof item === "object" && !Array.isArray(item) && (item as Record<string, JsonValue>).id === workerId
  );
  if (!match) {
    throw new Error(`Cloud worker not found: ${workerId}`);
  }
  if (options.json) {
    printJson(match);
    return;
  }
  console.log("Cloud log streaming is not available yet.");
  console.log("Worker status:");
  printSailList([match]);
}

function collectionFromResult(result: JsonValue, key: "sails" | "workspaces"): JsonValue[] {
  if (Array.isArray(result)) {
    return result;
  }
  if (result && typeof result === "object" && !Array.isArray(result)) {
    const collection = (result as Record<string, JsonValue>)[key];
    return Array.isArray(collection) ? collection : [];
  }
  return [];
}

async function listSails(options: CloudCommandOptions): Promise<void> {
  const client = await cloudClient({ requireToken: true });
  const result = await client.request<JsonValue>("/api/rudder/sail", { method: "GET" });
  await printResult(result, options);
}

async function status(options: CloudCommandOptions): Promise<void> {
  const client = await cloudClient({ requireToken: true });
  const state = await loadCloudAuth();
  const runtime = await selectedCloudRuntime();
  const sails = await client.request<JsonValue>("/api/rudder/sail", { method: "GET" });
  const sailRows = Array.isArray(sails)
    ? sails
    : sails && typeof sails === "object" && !Array.isArray(sails) && Array.isArray((sails as Record<string, JsonValue>).sails)
      ? (sails as Record<string, JsonValue>).sails as JsonValue[]
      : [];
  const sailCount = sailRows.length;
  const result: Record<string, JsonValue> = {
    ok: true,
    cloudUrl: client.baseUrl,
    runtime,
    sails: sailCount,
  };
  if (state?.cloudUrl === client.baseUrl) {
    if (state.email) {
      result.email = state.email;
    }
    if (state.accountId) {
      result.accountId = state.accountId;
    }
    if (state.byocSshHost) {
      result.byocSshHost = state.byocSshHost;
    }
  }
  if (options.json) {
    printJson(result);
    return;
  }
  console.log(`Logged in to ${client.baseUrl}${state?.email ? ` as ${state.email}` : ""}.`);
  console.log(`Runtime: ${runtime}`);
  if (state?.byocSshHost) {
    console.log(`BYOC SSH host: ${state.byocSshHost}`);
  }
  console.log(`Cloud workers: ${sailCount}`);
}

async function bootstrap(args: string[], options: CloudCommandOptions): Promise<void> {
  const sailId = args[0];
  if (!sailId) {
    throw new Error("Missing sail id. Usage: rudder cloud bootstrap <id>");
  }
  const client = await cloudClient({ requireToken: true });
  const result = await client.request<JsonValue>(`/api/rudder/sail/${encodeURIComponent(sailId)}/bootstrap`, {
    method: "POST",
    body: {},
  });
  await printResult(result, options);
}

async function mutateSail(action: "onload" | "pause" | "resume" | "stop", args: string[], options: CloudCommandOptions): Promise<void> {
  const sailId = args[0];
  if (!sailId) {
    throw new Error(`Missing sail id. Usage: rudder sail ${action} <id>`);
  }
  const client = await cloudClient({ requireToken: true });
  let result: JsonValue;
  try {
    result = await client.request<JsonValue>(`/api/rudder/sail/${encodeURIComponent(sailId)}/${action}`, {
      method: "POST",
      body: args.length > 1 ? { args: args.slice(1) } : {},
    });
  } catch (error) {
    if (action !== "onload" && isSailNotFound(error)) {
      await workspaceMutate(action, args, options);
      return;
    }
    throw error;
  }
  await printResult(result, options);
}

function isSailNotFound(error: unknown): boolean {
  const message = error instanceof Error ? error.message : String(error);
  return /sail not found/i.test(message);
}

async function talk(args: string[], options: CloudCommandOptions): Promise<void> {
  const sailId = args[0];
  const message = args.slice(1).join(" ").trim();
  if (!sailId || !message) {
    throw new Error('Usage: rudder cloud talk <id> "<message>"');
  }
  const client = await cloudClient({ requireToken: true });
  const result = await client.request<JsonValue>(
    `/api/rudder/sail/${encodeURIComponent(sailId)}/input`,
    { method: "POST", body: { text: message } },
  );
  if (options.json) {
    printJson(result);
    return;
  }
  const delivered = result && typeof result === "object" && !Array.isArray(result)
    ? (result as Record<string, JsonValue>).delivered === true
    : false;
  if (!delivered) {
    console.log(`Could not reach ${sailId}: it is not connected right now.`);
    return;
  }
  console.log(`→ ${sailId}: ${message}`);
  // Give the agent a moment, then show what it said back.
  await sleep(3500);
  await tailOutput([sailId], options);
}

async function tailOutput(args: string[], options: CloudCommandOptions): Promise<void> {
  const sailId = args[0];
  if (!sailId) {
    throw new Error("Usage: rudder cloud output <id>");
  }
  const client = await cloudClient({ requireToken: true });
  const result = await client.request<Record<string, JsonValue>>(
    `/api/rudder/sail/${encodeURIComponent(sailId)}/output`,
    { method: "GET" },
  );
  if (options.json) {
    printJson(result);
    return;
  }
  const output = typeof result.output === "string" ? result.output : "";
  const cleaned = stripAnsiForCli(output).trimEnd();
  const lines = cleaned ? cleaned.split("\n").slice(-30).join("\n") : "(no output yet)";
  console.log(lines);
}

function stripAnsiForCli(text: string): string {
  // eslint-disable-next-line no-control-regex
  return text
    .replace(/\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)/g, "")
    .replace(/\x1b\[[0-9;?]*[ -/]*[@-~]/g, "")
    .replace(/\x1b[@-Z\\-_=>()#][0-9;]*/g, "")
    .replace(/[\x00-\x08\x0b-\x1f\x7f]/g, "");
}

async function quickstart(options: CloudCommandOptions): Promise<void> {
  const url = normalizeCloudUrl(process.env.RUDDER_CLOUD_URL) || DEFAULT_CLOUD_URL;
  if (options.json) {
    printJson({ cloudUrl: url, steps: ["install", "login", "launch", "talk", "slack"] });
    return;
  }
  const lines = [
    "Rudder Cloud — copy/paste quickstart",
    "",
    "1. Install Rudder:",
    "   npm install -g @exla/rudder@latest",
    "",
    "2. Log in to the cloud control plane:",
    "   rudder login",
    "",
    "3. Launch a fresh cloud instance on any task (run inside a repo):",
    '   rudder cloud "fix the failing tests"',
    "",
    "4. See your instances and talk to any of them:",
    "   rudder cloud list",
    '   rudder cloud talk <id> "what are you working on?"',
    "   rudder cloud output <id>",
    "   rudder cloud attach <id>      # full interactive terminal",
    "",
    "5. Talk to every instance from Slack (one thread per instance):",
    "   rudder cloud slack            # prints the Slack setup",
    "",
    `Control plane: ${url}`,
  ];
  console.log(lines.join("\n"));
}

async function slackSetup(args: string[], options: CloudCommandOptions): Promise<void> {
  const url = normalizeCloudUrl(process.env.RUDDER_CLOUD_URL) || DEFAULT_CLOUD_URL;
  const eventsUrl = `${url.replace(/\/$/, "")}/api/slack/events`;
  const channel = process.env.RUDDER_SLACK_CHANNEL || "C0B78TDLM5G";
  const manifest = {
    display_information: { name: "Rudder Cloud" },
    features: {
      bot_user: { display_name: "rudder", always_online: true },
    },
    oauth_config: {
      scopes: { bot: ["app_mentions:read", "channels:history", "groups:history", "chat:write"] },
    },
    settings: {
      event_subscriptions: {
        request_url: eventsUrl,
        bot_events: ["app_mention", "message.channels", "message.groups"],
      },
      org_deploy_enabled: false,
      socket_mode_enabled: false,
    },
  };
  if (args[0] === "manifest") {
    console.log(JSON.stringify(manifest, null, 2));
    return;
  }
  if (options.json) {
    printJson({ eventsUrl, channel, manifest });
    return;
  }
  const lines = [
    "Rudder Cloud → Slack setup",
    "",
    `Channel: ${channel}`,
    `Events request URL: ${eventsUrl}`,
    "",
    "1. Create a Slack app from the manifest (api.slack.com/apps → Create New App → From a manifest):",
    "   rudder cloud slack manifest    # prints the JSON manifest to paste",
    "",
    "2. Install the app to your workspace and invite it to the channel:",
    `   /invite @rudder   (in ${channel})`,
    "",
    "3. Set these on the control plane (Fly secrets), then redeploy:",
    "   SLACK_BOT_TOKEN=xoxb-...        # Bot User OAuth Token",
    "   SLACK_SIGNING_SECRET=...        # Basic Information → Signing Secret",
    `   RUDDER_SLACK_CHANNEL=${channel}`,
    "",
    "   flyctl secrets set SLACK_BOT_TOKEN=xoxb-... SLACK_SIGNING_SECRET=... \\",
    `     RUDDER_SLACK_CHANNEL=${channel} -a rudder-cloud-control`,
    "",
    "That's it. Every `rudder cloud \"<task>\"` now opens a thread in the channel;",
    "reply in a thread to talk to that instance, or use `list` / `talk <id> <msg>`.",
  ];
  console.log(lines.join("\n"));
}

async function setupOAuthProvider(
  provider: "github" | "google",
  args: string[],
  options: CloudCommandOptions,
): Promise<void> {
  const envPrefix = provider === "github" ? "RUDDER_GITHUB" : "RUDDER_GOOGLE";
  const clientId = args[0]?.trim() || process.env[`${envPrefix}_CLIENT_ID`]?.trim();
  const clientSecret =
    process.env[`${envPrefix}_CLIENT_SECRET`]?.trim() ||
    args[1]?.trim() ||
    await promptSecret(`${provider === "github" ? "GitHub App" : "Google OAuth"} client secret`);
  if (!clientId || !clientSecret) {
    throw new Error([
      `Missing ${provider === "github" ? "GitHub" : "Google"} OAuth credentials.`,
      `Usage: rudder cloud setup-${provider} <client-id>`,
      `Or set ${envPrefix}_CLIENT_ID and ${envPrefix}_CLIENT_SECRET.`,
    ].join("\n"));
  }
  const client = await cloudClient({ requireToken: true });
  const result = await client.request<JsonValue>(`/api/rudder/setup/${provider}`, {
    method: "POST",
    body: {
      clientId,
      clientSecret,
    },
  });
  await printResult(result, options);
}

async function setupByoc(args: string[], options: CloudCommandOptions): Promise<void> {
  const sshConfigPath = path.join(os.homedir(), ".ssh", "config");
  const configuredHosts = await listSshConfigHosts(sshConfigPath);
  const host = (options.sshHost ?? args.join(" ").trim()) || await chooseByocHost(configuredHosts);
  if (!host) {
    throw new Error([
      "Missing BYOC SSH host.",
      "Add your workstation/server to ~/.ssh/config, then run:",
      "",
      "  rudder cloud byoc <ssh-host>",
      "",
      "Example ~/.ssh/config:",
      "  Host rudder-workstation",
      "    HostName 203.0.113.10",
      "    User ubuntu",
      "    IdentityFile ~/.ssh/id_ed25519",
      "",
      configuredHosts.length
        ? `Detected SSH hosts: ${configuredHosts.slice(0, 12).join(", ")}`
        : `No usable hosts found in ${shortenHome(sshConfigPath)}.`,
    ].join("\n"));
  }

  const configMentionsHost = configuredHosts.includes(host) || await sshConfigMentions(sshConfigPath, host);
  const diagnostics = await checkByocHost(host);
  const client = await cloudClient({ requireToken: true });
  const state = await loadCloudAuth();
  if (!state || state.cloudUrl !== client.baseUrl) {
    throw new Error("Not logged in to this Rudder Cloud control plane. Run `rudder login` first.");
  }
  await saveCloudAuth({
    ...state,
    defaultRuntime: state.defaultRuntime === "byo-vm" ? "fly" : state.defaultRuntime,
    byocSshHost: host,
    updatedAt: nowIso(),
  });

  if (options.json) {
    const result: Record<string, JsonValue> = {
      ok: true,
      cloudUrl: client.baseUrl,
      byocSshHost: host,
    };
    const defaultRuntime = state.defaultRuntime === "byo-vm" ? "fly" : state.defaultRuntime;
    if (defaultRuntime) {
      result.defaultRuntime = defaultRuntime;
    }
    printJson(result);
    return;
  }

  console.log(`Rudder BYOC host set to ${host}.`);
  console.log("Plain `rudder cloud` and dashboard `/cloud` continue to use Fly by default.");
  console.log("Use `rudder cloud vm <task>` when you want to run a task on this BYOC host.");

  if (!configMentionsHost) {
    console.log(`\nNote: ${shortenHome(sshConfigPath)} does not appear to define Host ${host}.`);
    console.log("Rudder can still use it if SSH resolves it, but a ~/.ssh/config entry is recommended:");
    console.log(`  Host ${host}`);
    console.log("    HostName <server-ip-or-dns>");
    console.log("    User <user>");
    console.log("    IdentityFile ~/.ssh/<private-key>");
  }
  if (diagnostics.ok) {
    console.log(`SSH check passed for ${host}. Docker is available on the BYOC host.`);
  } else {
    console.log(`\nSSH check did not fully pass for ${host}: ${diagnostics.message}`);
    console.log("Fix SSH/Docker before launching, or run the printed Docker command manually on that host.");
  }
}

async function chooseByocHost(hosts: string[]): Promise<string> {
  if (hosts.length === 0) {
    return await promptText("SSH host from ~/.ssh/config");
  }
  return await promptSelect(
    "Choose a BYOC SSH host from ~/.ssh/config",
    hosts.slice(0, 24).map((host) => ({ value: host, label: host })),
    hosts[0],
  );
}

async function configureDefaultRuntime(runtime: CloudRuntime, options: CloudCommandOptions, byocSshHost?: string): Promise<void> {
  const client = await cloudClient({ requireToken: true });
  const state = await loadCloudAuth();
  if (!state || state.cloudUrl !== client.baseUrl) {
    throw new Error("Not logged in to this Rudder Cloud control plane. Run `rudder login` first.");
  }
  await saveCloudAuth({
    ...state,
    defaultRuntime: runtime,
    byocSshHost: runtime === "byo-vm" ? byocSshHost ?? state.byocSshHost : undefined,
    updatedAt: nowIso(),
  });
  const result: Record<string, JsonValue> = {
    ok: true,
    cloudUrl: client.baseUrl,
    defaultRuntime: runtime,
  };
  const savedByocHost = byocSshHost ?? state.byocSshHost;
  if (runtime === "byo-vm" && savedByocHost) {
    result.byocSshHost = savedByocHost;
  }
  const envRuntime = envCloudRuntime();
  if (envRuntime) {
    result.envOverride = envRuntime;
  }
  if (options.json) {
    printJson(result);
    return;
  }
  console.log(`Rudder Cloud runtime set to ${runtime}.`);
  if (runtime === "byo-vm") {
    const host = byocSshHost ?? state.byocSshHost;
    console.log("Future `rudder cloud <task>` and `/sail <task>` launches will prepare a BYOC worker instead of creating a Fly Machine.");
    console.log(host
      ? `Rudder will try to start the worker over SSH on ${host}.`
      : "Run `rudder cloud byoc <ssh-host>` to let Rudder start workers over SSH.");
  } else {
    console.log("Future `rudder cloud <task>` and `/sail <task>` launches will create Fly Machines.");
  }
  if (envRuntime) {
    console.log(`RUDDER_CLOUD_RUNTIME=${envRuntime} is set and will override this saved default.`);
  }
}

async function runtime(args: string[], options: CloudCommandOptions): Promise<void> {
  const next = args[0] ? parseCloudRuntime(args[0]) : undefined;
  if (args[0] && !next) {
    throw new Error("Runtime must be `fly`, `byoc`, or `byo-vm`.");
  }
  if (next) {
    await configureDefaultRuntime(next, options);
    return;
  }
  const client = await cloudClient({ requireToken: true });
  const current = await selectedCloudRuntime();
  const state = await loadCloudAuth();
  const savedRuntime = parseCloudRuntime(state?.defaultRuntime);
  const result: Record<string, JsonValue> = {
    cloudUrl: client.baseUrl,
    runtime: current,
  };
  const envRuntime = envCloudRuntime();
  if (state?.cloudUrl === client.baseUrl && savedRuntime) {
    result.savedDefaultRuntime = savedRuntime;
  }
  if (state?.cloudUrl === client.baseUrl && state.byocSshHost) {
    result.byocSshHost = state.byocSshHost;
  }
  if (envRuntime) {
    result.envOverride = envRuntime;
  }
  if (options.json) {
    printJson(result);
  } else {
    console.log(`Rudder Cloud runtime: ${current}`);
    if (envRuntime) {
      console.log(`Set by RUDDER_CLOUD_RUNTIME=${envRuntime}.`);
    } else if (state?.cloudUrl === client.baseUrl && savedRuntime) {
      console.log("Set in local Rudder Cloud config.");
    } else {
      console.log("Using default Fly Machines runtime.");
    }
    if (state?.cloudUrl === client.baseUrl && state.byocSshHost) {
      console.log(`BYOC SSH host: ${state.byocSshHost}`);
    }
  }
}

async function sshConfigMentions(configPath: string, host: string): Promise<boolean> {
  const text = await fsp.readFile(configPath, "utf8").catch(() => "");
  if (!text.trim()) {
    return false;
  }
  const target = host.toLowerCase();
  for (const line of text.split(/\r?\n/)) {
    const trimmed = line.trim();
    if (!trimmed || trimmed.startsWith("#")) {
      continue;
    }
    const match = /^Host\s+(.+)$/i.exec(trimmed);
    if (!match) {
      continue;
    }
    const patterns = match[1].split(/\s+/).map((part) => part.toLowerCase());
    if (patterns.includes(target)) {
      return true;
    }
  }
  return false;
}

async function listSshConfigHosts(configPath: string): Promise<string[]> {
  const text = await fsp.readFile(configPath, "utf8").catch(() => "");
  const hosts: string[] = [];
  const seen = new Set<string>();
  for (const line of text.split(/\r?\n/)) {
    const trimmed = line.trim();
    if (!trimmed || trimmed.startsWith("#")) {
      continue;
    }
    const match = /^Host\s+(.+)$/i.exec(trimmed);
    if (!match) {
      continue;
    }
    for (const host of match[1].split(/\s+/)) {
      if (!host || host.includes("*") || host.includes("?") || host.startsWith("!")) {
        continue;
      }
      if (seen.has(host)) {
        continue;
      }
      seen.add(host);
      hosts.push(host);
    }
  }
  return hosts;
}

async function checkByocHost(host: string): Promise<{ ok: boolean; message: string }> {
  if (!commandExists("ssh")) {
    return { ok: false, message: "ssh is not installed or not on PATH" };
  }
  const result = await runCommand("ssh", [
    "-o",
    "BatchMode=yes",
    "-o",
    "ConnectTimeout=8",
    host,
    "command -v docker >/dev/null && docker info >/dev/null 2>&1",
  ], { allowFailure: true });
  if (result.code === 0) {
    return { ok: true, message: "ok" };
  }
  const detail = (result.stderr || result.stdout || `ssh exited ${result.code}`).trim();
  return { ok: false, message: detail };
}

async function startByocWorkerOverSsh(host: string, bootstrapCommand: string): Promise<void> {
  if (!commandExists("ssh")) {
    throw new Error("ssh is not installed or not on PATH");
  }
  const remoteCommand = [
    "mkdir -p ~/.rudder/byoc",
    `nohup sh -lc ${shellQuote(nonInteractiveDockerCommand(bootstrapCommand))} > ~/.rudder/byoc/worker.log 2>&1 < /dev/null &`,
  ].join(" && ");
  await runCommand("ssh", [
    "-o",
    "BatchMode=yes",
    "-o",
    "ConnectTimeout=10",
    host,
    remoteCommand,
  ]);
}

function nonInteractiveDockerCommand(command: string): string {
  return command
    .replace(/\bdocker run --rm -it\b/g, "docker run --rm")
    .replace(/\bdocker run --rm -i -t\b/g, "docker run --rm")
    .replace(/\bdocker run --rm -t -i\b/g, "docker run --rm");
}

async function cloudClient(options: { requireToken: boolean }): Promise<CloudClient> {
  const baseUrl = normalizeCloudUrl(process.env.RUDDER_CLOUD_URL);
  const state = await loadCloudAuth();
  const envToken = process.env.RUDDER_CLOUD_TOKEN?.trim();
  const token = envToken || (state?.cloudUrl === baseUrl ? state.token : undefined);
  if (options.requireToken && !token) {
    throw new Error("Not logged in to Rudder Cloud. Run `rudder login` first.");
  }
  return {
    baseUrl,
    async request<T>(pathOrUrl: string, init: { method: string; body?: JsonValue }) {
      const url = pathOrUrl.startsWith("http://") || pathOrUrl.startsWith("https://")
        ? pathOrUrl
        : new URL(pathOrUrl, `${baseUrl}/`).toString();
      const headers: Record<string, string> = {
        Accept: "application/json",
      };
      let body: string | undefined;
      if (init.body !== undefined) {
        headers["Content-Type"] = "application/json";
        body = JSON.stringify(init.body);
      }
      if (token) {
        headers.Authorization = `Bearer ${token}`;
      }
      const response = await fetch(url, {
        method: init.method,
        headers,
        body,
      });
      const text = await response.text();
      const parsed = text ? parseJson(text) : null;
      if (!response.ok) {
        const message = responseErrorMessage(parsed) || text.trim() || `${response.status} ${response.statusText}`;
        throw new Error(`Rudder Cloud request failed: ${message}`);
      }
      return parsed as T;
    },
    async download(pathname: string) {
      const response = await fetch(new URL(pathname, `${baseUrl}/`).toString(), { headers: token ? { Authorization: `Bearer ${token}` } : {} });
      if (!response.ok) {
        const text = await response.text();
        throw new Error(`Rudder Cloud request failed: ${responseErrorMessage(text ? parseJson(text) : null) || `${response.status} ${response.statusText}`}`);
      }
      return Buffer.from(await response.arrayBuffer());
    },
  };
}

function normalizeCloudUrl(raw: string | undefined): string {
  const value = raw?.trim() || DEFAULT_CLOUD_URL;
  if (!value) {
    throw new Error("RUDDER_CLOUD_URL is not configured. Set it to your Rudder Cloud control plane URL.");
  }
  let url: URL;
  try {
    url = new URL(value);
  } catch {
    throw new Error("RUDDER_CLOUD_URL must be a valid http(s) URL.");
  }
  if (url.protocol !== "http:" && url.protocol !== "https:") {
    throw new Error("RUDDER_CLOUD_URL must be a valid http(s) URL.");
  }
  // The control plane hands back a shell command (bootstrapCommand) that the client
  // can auto-run over SSH, so a plain-HTTP control plane is a MITM-to-RCE risk.
  // Require https except for loopback dev or an explicit opt-in.
  const isLoopback = url.hostname === "127.0.0.1" || url.hostname === "localhost" || url.hostname === "::1";
  if (url.protocol === "http:" && !isLoopback && process.env.RUDDER_CLOUD_ALLOW_HTTP !== "1") {
    throw new Error(
      "Refusing a plain-HTTP RUDDER_CLOUD_URL (it can be MITM'd into running arbitrary commands). " +
        "Use https://, or set RUDDER_CLOUD_ALLOW_HTTP=1 to override for a trusted local network.",
    );
  }
  url.hash = "";
  url.search = "";
  return url.toString().replace(/\/$/, "");
}

async function selectedCloudRuntime(explicit?: CloudRuntime): Promise<CloudRuntime> {
  if (explicit) {
    return explicit;
  }
  const envRuntime = envCloudRuntime();
  if (envRuntime) {
    return envRuntime;
  }
  const baseUrl = normalizeCloudUrl(process.env.RUDDER_CLOUD_URL);
  const state = await loadCloudAuth();
  const savedRuntime = parseCloudRuntime(state?.defaultRuntime);
  return state?.cloudUrl === baseUrl && savedRuntime ? savedRuntime : "fly";
}

function parseCloudRuntime(raw: string | undefined): CloudRuntime | undefined {
  const value = raw?.trim().toLowerCase();
  if (!value) {
    return undefined;
  }
  if (value === "fly" || value === "fly-machine" || value === "fly-machines") {
    return "fly";
  }
  if (value === "byo" || value === "byoc" || value === "byo-vm" || value === "manual" || value === "self-hosted" || value === "vm") {
    return "byo-vm";
  }
  return undefined;
}

function envCloudRuntime(): CloudRuntime | undefined {
  const runtime = parseCloudRuntime(process.env.RUDDER_CLOUD_RUNTIME);
  if (process.env.RUDDER_CLOUD_RUNTIME?.trim() && !runtime) {
    throw new Error("RUDDER_CLOUD_RUNTIME must be `fly`, `byoc`, or `byo-vm`.");
  }
  return runtime;
}

async function pollLogin(
  client: CloudClient,
  pollPath: string,
  deviceCode: string | undefined,
): Promise<LoginPollResponse> {
  if (pollPath.startsWith("http://") || pollPath.startsWith("https://") || !deviceCode) {
    return await client.request<LoginPollResponse>(pollPath, { method: "GET" });
  }
  return await client.request<LoginPollResponse>(pollPath, {
    method: "POST",
    body: { deviceCode },
  });
}

async function loadCloudAuth(): Promise<CloudAuthState | null> {
  const state = await readJson<CloudAuthState>(cloudAuthPath());
  return state?.version === 1 && typeof state.token === "string" ? state : null;
}

async function saveCloudAuth(state: CloudAuthState): Promise<void> {
  await writeJson(cloudAuthPath(), state, { mode: 0o600 });
}

async function createSnapshot(repoRoot: string, requestedHomePaths: string[], options: SnapshotOptions = {}): Promise<{
  tempDir: string;
  archivePath: string;
  manifest: SnapshotManifest;
}> {
  const tempDir = await fsp.mkdtemp(path.join(os.tmpdir(), "rudder-cloud-"));
  const stageDir = path.join(tempDir, "snapshot");
  const repoStage = path.join(stageDir, "repo");
  const homeStage = path.join(stageDir, "home");
  await ensureDir(repoStage);
  await copyRepoFiles(repoRoot, repoStage);
  const projectEnvFiles = options.migration
    ? await copyProjectEnvFiles(repoRoot, repoStage)
    : 0;
  const rudderState = options.includeRudderState || (options.rudderStateRunIds?.length ?? 0) > 0
    ? await copyRudderState(repoRoot, repoStage, options.includeRudderState ? undefined : options.rudderStateRunIds)
    : undefined;

  const includeCredentials = options.includeCredentials !== false;
  const includedHomePaths: string[] = [];
  let capturedEnvCount = 0;
  if (includeCredentials) {
    const homePaths = normalizeHomePaths(requestedHomePaths);
    for (const homePath of homePaths) {
      const copied = await copyHomePath(homePath, homeStage);
      if (copied) {
        includedHomePaths.push(shortenHome(homePath));
      }
    }

    // On macOS, Claude Code stores its OAuth token in the Keychain rather than
    // ~/.claude/.credentials.json, so the home-paths copy above doesn't pick it
    // up. Extract it from the Keychain and stage it as a credentials file so
    // the cloud worker boots already logged in.
    if (await stageClaudeKeychainCredentials(homeStage)) {
      includedHomePaths.push("~/.claude/.credentials.json (keychain)");
    }

    const capturedEnv = captureCloudEnv(Boolean(options.migration));
    if (Object.keys(capturedEnv).length > 0) {
      await ensureDir(path.join(stageDir, "env"));
      await writeJson(path.join(stageDir, "env", "cloud-env.json"), capturedEnv as unknown as JsonValue);
      capturedEnvCount = Object.keys(capturedEnv).length;
    }
  }

  let migratedAgentsCount = 0;
  if (options.migration && options.migration.plan.migrated.length > 0) {
    const entries = await stageMigratedAgents(
      stageDir,
      repoRoot,
      options.migration.repoName,
      options.migration.plan.migrated,
    );
    const migrationManifest: MigrationSnapshotManifest = {
      version: 1,
      createdAt: nowIso(),
      agents: entries,
    };
    await writeJson(path.join(stageDir, "migration.json"), migrationManifest as unknown as JsonValue);
    migratedAgentsCount = entries.length;
  }

  const manifest: SnapshotManifest = {
    version: 1,
    createdAt: nowIso(),
    repo: {
      root: path.basename(repoRoot),
      branch: await currentBranch(repoRoot),
      commit: await currentCommit(repoRoot),
    },
    homePaths: includedHomePaths,
    ...(rudderState ? { rudderState } : {}),
    ...(migratedAgentsCount > 0 ? { migratedAgents: migratedAgentsCount } : {}),
    ...(capturedEnvCount > 0 ? { capturedEnvVars: capturedEnvCount } : {}),
    ...(projectEnvFiles > 0 ? { projectEnvFiles } : {}),
  };
  await writeJson(path.join(stageDir, "manifest.json"), manifest);

  const archivePath = path.join(tempDir, `${newRunId("cloud-snapshot")}.tgz`);
  await runCommand("tar", ["-czf", archivePath, "-C", stageDir, "."], { cwd: stageDir });
  return { tempDir, archivePath, manifest };
}

const CLOUD_ENV_DEFAULT_NAMES = new Set([
  "ANTHROPIC_API_KEY",
  "ANTHROPIC_AUTH_TOKEN",
  "OPENAI_API_KEY",
  "GOOGLE_API_KEY",
  "AWS_ACCESS_KEY_ID",
  "AWS_SECRET_ACCESS_KEY",
  "AWS_SESSION_TOKEN",
  "AWS_REGION",
  "AWS_DEFAULT_REGION",
  "AWS_PROFILE",
  "VERCEL_TOKEN",
  "VERCEL_ORG_ID",
  "VERCEL_PROJECT_ID",
  "NETLIFY_AUTH_TOKEN",
  "GITHUB_TOKEN",
  "GH_TOKEN",
  "GITLAB_TOKEN",
  "NPM_TOKEN",
  "CARGO_REGISTRY_TOKEN",
  "HUGGING_FACE_HUB_TOKEN",
  "HF_TOKEN",
  "DATABASE_URL",
  "REDIS_URL",
  "POSTGRES_URL",
  "STRIPE_API_KEY",
  "STRIPE_SECRET_KEY",
  "SLACK_BOT_TOKEN",
  "DISCORD_TOKEN",
]);
const CLOUD_ENV_SUFFIX_PATTERNS = [/_API_KEY$/, /_AUTH_TOKEN$/, /_ACCESS_TOKEN$/, /_TOKEN$/, /_SECRET$/, /_SECRET_KEY$/];
const CLOUD_ENV_BLOCKLIST = new Set([
  // Things we explicitly do not want shipping
  "PATH",
  "HOME",
  "USER",
  "PWD",
  "SHELL",
  "TERM",
  "TMPDIR",
  "LOGNAME",
  "SHLVL",
  "OLDPWD",
  "DISPLAY",
  "EDITOR",
  "PAGER",
  "LANG",
  "LC_ALL",
  // Rudder-internal vars that are set by the worker itself
  "RUDDER_WORKSPACE_ID",
  "RUDDER_SAIL_ID",
  "RUDDER_WORKER_TOKEN",
  "RUDDER_CLOUD_URL",
  "RUDDER_SNAPSHOT_URL",
  "RUDDER_CLOUD_TOKEN",
  "RUDDER_CLOUD_ENV_VARS",
  "RUDDER_CLOUD_ENV_BLOCKLIST",
  "RUDDER_TASK",
  "RUDDER_REPO_NAME",
  "RUDDER_ACCOUNT_ID",
  "RUDDER_HANDOFF_PATH",
]);

export function captureCloudEnv(includeAll = false): Record<string, string> {
  const extra = (process.env.RUDDER_CLOUD_ENV_VARS || "")
    .split(",")
    .map((name) => name.trim())
    .filter(Boolean);
  const blockExtra = (process.env.RUDDER_CLOUD_ENV_BLOCKLIST || "")
    .split(",")
    .map((name) => name.trim())
    .filter(Boolean);
  const blocked = new Set([...CLOUD_ENV_BLOCKLIST, ...blockExtra]);
  const out: Record<string, string> = {};
  for (const [name, value] of Object.entries(process.env)) {
    if (!name || typeof value !== "string" || !value) {
      continue;
    }
    if (blocked.has(name)) {
      continue;
    }
    const matches = includeAll
      || CLOUD_ENV_DEFAULT_NAMES.has(name)
      || CLOUD_ENV_SUFFIX_PATTERNS.some((pattern) => pattern.test(name))
      || extra.includes(name);
    if (!matches) {
      continue;
    }
    out[name] = value;
  }
  return out;
}

async function stageMigratedAgents(
  stageDir: string,
  repoRoot: string,
  repoName: string,
  migrated: MigrationCandidate[],
): Promise<MigrationManifestEntry[]> {
  const workspacesStage = path.join(stageDir, "migrated-workspaces");
  const sessionsStage = path.join(stageDir, "migrated-sessions");
  const entries: MigrationManifestEntry[] = [];
  for (const candidate of migrated) {
    if (!(await pathExists(candidate.workspacePath))) {
      continue;
    }
    const hasSession = Boolean(
      candidate.sessionId
        && candidate.sessionJsonlPath
        && (await pathExists(candidate.sessionJsonlPath)),
    );
    const workspaceDest = path.join(workspacesStage, candidate.runId);
    await ensureDir(workspaceDest);
    await copyWorkspaceFiles(candidate.workspacePath, workspaceDest, repoRoot);
    await copyProjectEnvFiles(candidate.workspacePath, workspaceDest);
    if (path.resolve(candidate.workspacePath) !== path.resolve(repoRoot)) {
      // Ignored dotenv files do not normally exist in a jj workspace. Overlay
      // the source repo's project environment into every migrated worker.
      await copyProjectEnvFiles(repoRoot, workspaceDest);
    }
    let sessionJsonlSnapshotPath: string | undefined;
    if (hasSession) {
      const jsonlDest = path.join(sessionsStage, `${candidate.runId}.jsonl`);
      await ensureDir(path.dirname(jsonlDest));
      await fsp.cp(candidate.sessionJsonlPath!, jsonlDest, { force: true });
      sessionJsonlSnapshotPath = path.posix.join("migrated-sessions", `${candidate.runId}.jsonl`);
    }
    const cloudWorkspaceAbs = cloudWorkspaceAbsolutePath(repoName, candidate.runId, candidate.task);
    // For fresh restarts, build a prompt-engineered handoff from the local
    // run record so the new agent gets context instead of just the bare task.
    let freshPrompt: string | undefined;
    if (!hasSession) {
      const runJsonPath = path.join(repoRoot, ".rudder", "runs", candidate.runId, "run.json");
      const record = await readJson<{ turns?: Array<{ prompt: string; source: string }> }>(runJsonPath);
      const turns = Array.isArray(record?.turns) ? record!.turns : [];
      freshPrompt = buildFreshHandoffPrompt(candidate, turns);
    }
    entries.push({
      runId: candidate.runId,
      task: candidate.task,
      taskSummary: candidate.taskSummary,
      backend: candidate.backend,
      sessionId: candidate.sessionId ?? "",
      localWorkspacePath: candidate.workspacePath,
      cloudWorkspaceRelativePath: cloudWorkspaceAbs,
      sessionJsonlSnapshotPath: sessionJsonlSnapshotPath ?? "",
      workspaceBranch: candidate.workspaceBranch,
      freshPrompt,
    });
  }
  return entries;
}

async function copyWorkspaceFiles(workspacePath: string, target: string, _repoRoot: string): Promise<void> {
  const result = await runCommand("git", ["ls-files", "-z", "--cached", "--others", "--exclude-standard"], {
    cwd: workspacePath,
    allowFailure: true,
  });
  const files = result.code === 0
    ? result.stdout.split("\0").filter(Boolean)
    : await listFiles(workspacePath);
  for (const relative of files) {
    if (!relative || relative.startsWith(".git/") || relative === ".git" || relative.startsWith(".rudder/")) {
      continue;
    }
    const source = path.join(workspacePath, relative);
    const dest = path.join(target, relative);
    if (!isInside(workspacePath, source) || !isInside(target, dest)) {
      continue;
    }
    const stat = await fsp.lstat(source).catch(() => null);
    if (!stat || stat.isDirectory() || !(await shouldIncludeSnapshotPath(source))) {
      continue;
    }
    await ensureDir(path.dirname(dest));
    await fsp.cp(source, dest, { dereference: false, force: true });
  }
}

/** Copy project dotenv files only for explicit multi-agent workspace migration. */
export async function copyProjectEnvFiles(sourceRoot: string, targetRoot: string): Promise<number> {
  let copied = 0;
  const ignoredDirs = new Set([".git", ".jj", ".rudder", ".rudder-workspaces", ".rudder-worktrees", "node_modules"]);
  async function walk(current: string): Promise<void> {
    const entries = await fsp.readdir(current, { withFileTypes: true }).catch(() => []);
    for (const entry of entries) {
      if (entry.isDirectory()) {
        if (!ignoredDirs.has(entry.name)) {
          await walk(path.join(current, entry.name));
        }
        continue;
      }
      if (!entry.isFile() || !/^\.env(?:\..+)?$/.test(entry.name)) {
        continue;
      }
      const source = path.join(current, entry.name);
      const relative = path.relative(sourceRoot, source);
      const target = path.join(targetRoot, relative);
      if (!isInside(sourceRoot, source) || !isInside(targetRoot, target)) {
        continue;
      }
      const stat = await fsp.stat(source).catch(() => null);
      if (!stat || stat.size > MAX_HOME_SECRET_SCAN_BYTES) {
        continue;
      }
      await ensureDir(path.dirname(target));
      await fsp.cp(source, target, { force: true });
      copied += 1;
    }
  }
  await walk(sourceRoot);
  return copied;
}

export async function copyRudderState(repoRoot: string, repoStage: string, onlyRuns?: string[]): Promise<{ runs: number; files: string[] }> {
  const copied: string[] = [];
  const rudderMd = path.join(repoRoot, "RUDDER.md");
  if (await pathExists(rudderMd)) {
    const target = path.join(repoStage, "RUDDER.md");
    await fsp.cp(rudderMd, target, { force: true });
    copied.push("RUDDER.md");
  }

  const runsDir = path.join(repoRoot, ".rudder", "runs");
  const entries = await fsp.readdir(runsDir, { withFileTypes: true }).catch(() => []);
  let runs = 0;
  for (const entry of entries) {
    if (!entry.isDirectory() || entry.name.includes("/") || entry.name.includes("\\")) {
      continue;
    }
    if (onlyRuns && !onlyRuns.includes(entry.name)) {
      continue;
    }
    const runJson = path.join(runsDir, entry.name, "run.json");
    if (!(await pathExists(runJson))) {
      continue;
    }
    const relative = path.join(".rudder", "runs", entry.name, "run.json");
    const target = path.join(repoStage, relative);
    if (!isInside(repoRoot, runJson) || !isInside(repoStage, target)) {
      continue;
    }
    await ensureDir(path.dirname(target));
    await fsp.cp(runJson, target, { force: true });
    copied.push(relative);
    runs += 1;
  }
  return { runs, files: copied };
}

async function copyRepoFiles(repoRoot: string, repoStage: string): Promise<void> {
  const result = await runCommand("git", ["ls-files", "-z", "--cached", "--others", "--exclude-standard"], {
    cwd: repoRoot,
    allowFailure: true,
  });
  const files = result.code === 0
    ? result.stdout.split("\0").filter(Boolean)
    : await listFiles(repoRoot);
  for (const relative of files) {
    if (!relative || relative.startsWith(".git/") || relative.startsWith(".rudder/")) {
      continue;
    }
    const source = path.join(repoRoot, relative);
    const target = path.join(repoStage, relative);
    if (!isInside(repoRoot, source) || !isInside(repoStage, target)) {
      continue;
    }
    const stat = await fsp.lstat(source).catch(() => null);
    if (!stat || stat.isDirectory() || !(await shouldIncludeSnapshotPath(source))) {
      continue;
    }
    await ensureDir(path.dirname(target));
    await fsp.cp(source, target, { dereference: false, force: true });
  }
}

async function listFiles(dir: string): Promise<string[]> {
  const files: string[] = [];
  async function walk(current: string): Promise<void> {
    const entries = await fsp.readdir(current, { withFileTypes: true }).catch(() => []);
    for (const entry of entries) {
      if (entry.name === ".git" || entry.name === ".rudder" || entry.name === "node_modules") {
        continue;
      }
      const full = path.join(current, entry.name);
      if (entry.isDirectory()) {
        await walk(full);
      } else {
        files.push(path.relative(dir, full));
      }
    }
  }
  await walk(dir);
  return files;
}

function normalizeHomePaths(requested: string[]): string[] {
  const raw = [
    ...DEFAULT_HOME_PATHS,
    ...requested,
    ...(process.env.RUDDER_CLOUD_HOME_PATHS?.split(",") ?? []),
  ];
  const home = os.homedir();
  const seen = new Set<string>();
  const paths: string[] = [];
  for (const item of raw) {
    const trimmed = item.trim();
    if (!trimmed) {
      continue;
    }
    const resolved = path.resolve(expandHome(trimmed));
    if (!isInside(home, resolved) || seen.has(resolved)) {
      continue;
    }
    seen.add(resolved);
    paths.push(resolved);
  }
  return paths;
}

// On macOS, Claude Code keeps its OAuth token in the Keychain instead of
// ~/.claude/.credentials.json. Read it so cloud workspaces boot logged in.
async function readClaudeKeychainCredentials(): Promise<string | null> {
  if (process.platform !== "darwin") {
    return null;
  }
  if (!commandExists("security")) {
    return null;
  }
  const result = await runCommand(
    "security",
    ["find-generic-password", "-s", "Claude Code-credentials", "-w"],
    { allowFailure: true },
  );
  if (result.code !== 0) {
    return null;
  }
  const payload = result.stdout.trim();
  if (!payload || !payload.startsWith("{")) {
    return null;
  }
  try {
    JSON.parse(payload);
  } catch {
    return null;
  }
  return payload;
}

async function stageClaudeKeychainCredentials(homeStage: string): Promise<boolean> {
  const payload = await readClaudeKeychainCredentials();
  if (!payload) {
    return false;
  }
  const targetDir = path.join(homeStage, ".claude");
  await ensureDir(targetDir);
  await fsp.writeFile(path.join(targetDir, ".credentials.json"), payload + "\n", { mode: 0o600 });
  return true;
}

async function copyHomePath(source: string, homeStage: string): Promise<boolean> {
  if (!(await pathExists(source)) || !(await shouldIncludeSnapshotPath(source))) {
    return false;
  }
  const relative = path.relative(os.homedir(), source);
  const target = path.join(homeStage, relative);
  if (!isInside(homeStage, target)) {
    return false;
  }
  await fsp.cp(source, target, {
    dereference: false,
    recursive: true,
    force: true,
    filter: async (candidate) => await shouldIncludeSnapshotPath(candidate),
  });
  return true;
}

async function shouldIncludeSnapshotPath(candidate: string): Promise<boolean> {
  const normalized = path.resolve(candidate);
  const parts = normalized.split(path.sep).map((part) => part.toLowerCase());
  const basename = path.basename(normalized).toLowerCase();
  const explicitAwsCredentials = normalized === path.join(os.homedir(), ".aws", "credentials");
  if (basename.startsWith("._")) {
    return false;
  }
  if (parts.some((part) => SECRET_PATH_PARTS.has(part)) || SECRET_BASENAMES.has(basename)) {
    return false;
  }
  if (isInside(os.homedir(), normalized)) {
    if (parts.some((part) => BULKY_HOME_PATH_PARTS.has(part))) {
      return false;
    }
    if (BULKY_HOME_BASENAME_PATTERNS.some((pattern) => pattern.test(basename))) {
      return false;
    }
  }
  const stat = await fsp.lstat(normalized).catch(() => null);
  if (!stat || !stat.isFile() || stat.size > MAX_HOME_SECRET_SCAN_BYTES) {
    return true;
  }
  if (explicitAwsCredentials) {
    return true;
  }
  const text = await fsp.readFile(normalized, "utf8").catch(() => "");
  return !/(aws_access_key_id|aws_secret_access_key|aws_session_token|AWS_ACCESS_KEY_ID|AWS_SECRET_ACCESS_KEY|AWS_SESSION_TOKEN)/.test(text);
}

function isInside(parent: string, child: string): boolean {
  const relative = path.relative(path.resolve(parent), path.resolve(child));
  return relative === "" || (!relative.startsWith("..") && !path.isAbsolute(relative));
}

function openBrowser(url: string): void {
  const platform = process.platform;
  const command = platform === "darwin" ? "open" : platform === "win32" ? "cmd" : "xdg-open";
  const args = platform === "win32" ? ["/c", "start", "", url] : [url];
  const child = spawn(command, args, {
    detached: true,
    stdio: "ignore",
  });
  child.on("error", () => undefined);
  child.unref();
}

function withQuery(baseUrl: string, pathname: string, query: Record<string, string>): string {
  const url = new URL(pathname, `${baseUrl}/`);
  for (const [key, value] of Object.entries(query)) {
    url.searchParams.set(key, value);
  }
  return url.toString();
}

function parseJson(text: string): JsonValue {
  try {
    return JSON.parse(text) as JsonValue;
  } catch {
    return text;
  }
}

function responseErrorMessage(value: JsonValue | null): string | undefined {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    return undefined;
  }
  const record = value as Record<string, JsonValue>;
  return typeof record.error === "string"
    ? record.error
    : typeof record.message === "string"
      ? record.message
      : undefined;
}

async function printResult(result: JsonValue, options: CloudCommandOptions): Promise<void> {
  if (options.json) {
    printJson(result);
    return;
  }
  if (Array.isArray(result)) {
    printSailList(result);
    return;
  }
  if (result && typeof result === "object" && !Array.isArray(result)) {
    const record = result as Record<string, JsonValue>;
    if (typeof record.bootstrapCommand === "string") {
      const id = typeof record.id === "string" ? record.id : "BYOC sail";
      const status = typeof record.status === "string" ? record.status : undefined;
      const state = await loadCloudAuth();
      const host = options.sshHost ?? state?.byocSshHost;
      console.log(`${id}${status ? ` (${status})` : ""} is ready for BYOC.`);
      // bootstrapCommand is a shell command supplied by the control plane. Auto-running
      // it over SSH is RCE-on-your-host if the control plane is compromised or MITM'd,
      // so autostart is OPT-IN (RUDDER_BYOC_AUTOSTART=1) and the command is always
      // printed first so the user can review exactly what would run.
      console.log("BYOC bootstrap command (review before running):");
      console.log(record.bootstrapCommand);
      if (host && process.env.RUDDER_BYOC_AUTOSTART === "1") {
        try {
          console.log(`\nRUDDER_BYOC_AUTOSTART=1 set — running the above over SSH on ${host}...`);
          await startByocWorkerOverSsh(host, record.bootstrapCommand);
          console.log(`Started BYOC worker over SSH on ${host}.`);
          console.log(`Remote log: ssh ${host} 'tail -f ~/.rudder/byoc/worker.log'`);
        } catch (error) {
          console.log(`Could not start BYOC worker over SSH on ${host}: ${error instanceof Error ? error.message : String(error)}`);
          console.log("Run the command above manually on your workstation/server.");
        }
      } else {
        console.log("\nRun the command above on your workstation/server.");
        if (host) {
          console.log("To auto-run it over SSH next time, set RUDDER_BYOC_AUTOSTART=1.");
        } else {
          console.log("Tip: run `rudder cloud byoc <ssh-host>` to target an SSH host.");
        }
      }
      if (typeof record.updatedAt === "string") {
        console.log(`\nIf the command expires, run: rudder cloud bootstrap ${id}`);
      }
      return;
    }
    const sails = record.sails ?? record.items;
    if (Array.isArray(sails)) {
      printSailList(sails);
      return;
    }
    if (typeof record.id === "string" && (typeof record.status === "string" || typeof record.runtime === "string")) {
      const parts = [
        record.id,
        typeof record.status === "string" ? record.status : undefined,
        typeof record.runtime === "string" ? record.runtime : undefined,
        typeof record.repoName === "string" ? record.repoName : undefined,
      ].filter(Boolean);
      console.log(parts.join("  "));
      if (record.workspace === true || (record.run === null && typeof record.task !== "string")) {
        console.log("Rudder workspace uploaded. Use /cloud list to track it.");
      } else {
        console.log("Cloud worker created. Use /cloud list to track it.");
      }
      return;
    }
  }
  console.log(JSON.stringify(result, null, 2));
}

function printSailList(items: JsonValue[]): void {
  if (items.length === 0) {
    console.log("No cloud sails.");
    return;
  }
  for (const item of items) {
    if (!item || typeof item !== "object" || Array.isArray(item)) {
      console.log(String(item));
      continue;
    }
    const sail = item as CloudSail;
    console.log([
      sail.id,
      sail.status,
      sail.runtime,
      typeof sail.task === "string" && sail.task ? sail.task : undefined,
      typeof sail.repoName === "string" && sail.repoName ? sail.repoName : undefined,
      sail.branch,
      sail.url,
      sail.updatedAt ?? sail.createdAt,
    ].filter(Boolean).join("  "));
    if (sail.status === "failed" && typeof (sail as { failureReason?: unknown }).failureReason === "string") {
      console.log(`  why: ${(sail as { failureReason?: string }).failureReason}`);
    }
  }
}

function printJson(value: JsonValue): void {
  console.log(JSON.stringify(value, null, 2));
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

async function workspaceCommand(args: string[], options: CloudCommandOptions): Promise<void> {
  const sub = args[0] ?? "";
  const rest = args.slice(1);
  if (sub === "" || sub === "attach") {
    await workspaceAttach(rest, options);
    return;
  }
  if (sub === "create") {
    await workspaceCreate(rest, options);
    return;
  }
  if (sub === "share") {
    await workspaceShare(options);
    return;
  }
  if (sub === "status") {
    await workspaceStatus(rest, options);
    return;
  }
  if (sub === "stop" || sub === "pause" || sub === "resume") {
    await workspaceMutate(sub, rest, options);
    return;
  }
  if (sub === "list" || sub === "ls") {
    await workspaceList(options);
    return;
  }
  if (sub === "delete" || sub === "rm") {
    await workspaceDelete(rest, options);
    return;
  }
  throw new Error("Usage: rudder cloud workspace [attach [id|owner/repo]|create <owner/repo>|share|status|pause|resume|stop|delete [id]|list]");
}

const GITHUB_SLUG_RE = /^[\w.-]+\/[\w.-]+$/;

async function githubSlugFromOrigin(repoRoot: string): Promise<string | null> {
  const result = await runCommand("git", ["remote", "get-url", "origin"], {
    cwd: repoRoot,
    allowFailure: true,
  });
  if (result.code !== 0) {
    return null;
  }
  const match = result.stdout.trim().match(/github\.com[:/]([\w.-]+\/[\w.-]+?)(?:\.git)?$/);
  return match ? match[1] : null;
}

// Cloud-native workspace: the worker clones the repo from GitHub directly —
// full history, real origin remote, no local-directory upload.
async function workspaceCreate(args: string[], options: CloudCommandOptions): Promise<void> {
  const slug = (args[0] ?? "").trim().replace(/\.git$/, "");
  if (!GITHUB_SLUG_RE.test(slug)) {
    throw new Error("Usage: rudder cloud workspace create <owner/repo> [--branch <name>] [--region <code>]");
  }
  let branch: string | undefined;
  let region: string | undefined;
  for (let i = 1; i < args.length; i += 1) {
    const arg = args[i] ?? "";
    if (arg === "--branch") branch = args[++i];
    else if (arg.startsWith("--branch=")) branch = arg.slice("--branch=".length);
    else if (arg === "--region") region = args[++i];
    else if (arg.startsWith("--region=")) region = arg.slice("--region=".length);
    else throw new Error(`Unknown option: ${arg}`);
  }
  const client = await cloudClient({ requireToken: true });
  // Preflight: private clones and pushes need GitHub credentials from the vault.
  try {
    const secretsResult = await client.request<CloudSecretsListResponse>("/api/rudder/secrets", { method: "GET" });
    const secrets = secretsResult?.secrets ?? [];
    const hasGitCreds = secrets.some((secret) =>
      (secret.kind === "file" && secret.name.startsWith("~/.config/gh/"))
      || (secret.kind === "env" && (secret.name === "GITHUB_TOKEN" || secret.name === "GH_TOKEN")));
    if (!hasGitCreds && !options.json) {
      process.stderr.write(
        "Warning: no GitHub credentials in the cloud vault; private repos will fail to clone. Run `rudder cloud secrets sync` first.\n",
      );
    }
  } catch {
    // Old server or unconfigured vault; the worker will report clone failures.
  }
  if (!options.json) {
    process.stderr.write(`Creating cloud workspace for ${slug}...\n`);
  }
  const effectiveRegion = region ?? await explicitCloudRegion();
  const result = await client.request<JsonValue>("/api/rudder/workspace/create", {
    method: "POST",
    body: {
      repo: slug,
      ...(branch ? { branch } : {}),
      ...(effectiveRegion ? { region: effectiveRegion } : {}),
    },
  });
  await attachToWorkspaceResult(result, options);
}

async function workspaceAttachByRepo(slug: string, options: CloudCommandOptions): Promise<void> {
  const normalized = slug.trim().replace(/\.git$/, "");
  const client = await cloudClient({ requireToken: true });
  try {
    await client.request<JsonValue>(
      `/api/rudder/workspace/lookup?repo=${encodeURIComponent(normalized)}`,
      { method: "GET" },
    );
  } catch {
    throw new Error(
      `No cloud workspace for ${normalized}. Create one with \`rudder cloud workspace create ${normalized}\`.`,
    );
  }
  // The create endpoint reuses/warm-restarts an existing clone workspace.
  const result = await client.request<JsonValue>("/api/rudder/workspace/create", {
    method: "POST",
    body: { repo: normalized },
  });
  await attachToWorkspaceResult(result, options);
}

type CloudSecretMetadata = {
  name: string;
  kind: "env" | "file";
  filePath?: string;
  sizeBytes: number;
  sha256: string;
  source?: string;
  updatedAt: string;
};

type CloudSecretsListResponse = {
  secrets?: CloudSecretMetadata[];
  secretsVersion?: number;
};

const MAX_SECRET_VALUE_BYTES = 1024 * 1024;

async function secretsCommand(args: string[], options: CloudCommandOptions): Promise<void> {
  const sub = args[0] ?? "";
  const rest = args.slice(1);
  switch (sub) {
    case "set":
      await secretsSet(rest, options);
      return;
    case "list":
    case "ls":
      await secretsList(options);
      return;
    case "rm":
    case "remove":
    case "delete":
      await secretsRm(rest, options);
      return;
    case "sync":
      await secretsSync(options);
      return;
    default:
      throw new Error(
        "Usage: rudder cloud secrets [set <NAME> [value] | set --file <~/path> [source] | list | rm <NAME> | sync]",
      );
  }
}

// Convert an absolute or ~-prefixed path into the canonical tilde form the
// vault stores file secrets under. Only paths inside $HOME are allowed.
function toTildePath(input: string): string {
  const trimmed = input.trim();
  const resolved = path.resolve(expandHome(trimmed));
  const home = os.homedir();
  if (!isInside(home, resolved) || resolved === home) {
    throw new Error(`File secrets must live inside your home directory: ${input}`);
  }
  return `~/${path.relative(home, resolved).split(path.sep).join("/")}`;
}

async function readStdinAll(): Promise<string> {
  const chunks: Buffer[] = [];
  for await (const chunk of process.stdin) {
    chunks.push(Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk));
  }
  return Buffer.concat(chunks).toString("utf8");
}

async function secretsSet(args: string[], options: CloudCommandOptions): Promise<void> {
  const client = await cloudClient({ requireToken: true });
  if (args[0] === "--file") {
    const target = args[1];
    if (!target) {
      throw new Error("Usage: rudder cloud secrets set --file <~/path> [localSourcePath]");
    }
    const tildePath = toTildePath(target);
    const sourcePath = path.resolve(expandHome(args[2] ?? target));
    const content = await fsp.readFile(sourcePath);
    if (content.length > MAX_SECRET_VALUE_BYTES) {
      throw new Error(`${sourcePath} is ${content.length} bytes; file secrets are capped at ${MAX_SECRET_VALUE_BYTES}`);
    }
    await client.request<JsonValue>("/api/rudder/secrets/item", {
      method: "PUT",
      body: {
        name: tildePath,
        kind: "file",
        filePath: tildePath,
        valueBase64: content.toString("base64"),
        source: "manual",
      },
    });
    if (options.json) {
      printJson({ ok: true, name: tildePath, kind: "file" });
    } else {
      console.log(`Stored file secret ${tildePath} (${content.length} bytes). Takes effect on next workspace boot.`);
    }
    return;
  }
  const name = args[0];
  if (!name) {
    throw new Error("Usage: rudder cloud secrets set <NAME> [value] (or pipe the value on stdin)");
  }
  let value = args[1];
  if (value === undefined) {
    value = process.stdin.isTTY
      ? await promptSecret(`Value for ${name}`)
      : (await readStdinAll()).replace(/\r?\n$/, "");
  }
  if (!value) {
    throw new Error(`No value provided for ${name}.`);
  }
  if (Buffer.byteLength(value, "utf8") > MAX_SECRET_VALUE_BYTES) {
    throw new Error(`Value for ${name} exceeds the ${MAX_SECRET_VALUE_BYTES}-byte cap`);
  }
  await client.request<JsonValue>("/api/rudder/secrets/item", {
    method: "PUT",
    body: {
      name,
      kind: "env",
      valueBase64: Buffer.from(value, "utf8").toString("base64"),
      source: "manual",
    },
  });
  if (options.json) {
    printJson({ ok: true, name, kind: "env" });
  } else {
    console.log(`Stored env secret ${name}. Takes effect on next workspace boot.`);
  }
}

async function secretsList(options: CloudCommandOptions): Promise<void> {
  const client = await cloudClient({ requireToken: true });
  const result = await client.request<CloudSecretsListResponse>("/api/rudder/secrets", { method: "GET" });
  const secrets = result?.secrets ?? [];
  if (options.json) {
    printJson(result as JsonValue);
    return;
  }
  if (secrets.length === 0) {
    console.log("No cloud secrets stored. Run `rudder cloud secrets sync` to import your local credentials.");
    return;
  }
  const nameWidth = Math.max(4, ...secrets.map((secret) => secret.name.length));
  console.log(`${"NAME".padEnd(nameWidth)}  KIND  SIZE      UPDATED`);
  for (const secret of secrets) {
    const size = `${secret.sizeBytes}B`.padEnd(8);
    console.log(`${secret.name.padEnd(nameWidth)}  ${secret.kind.padEnd(4)}  ${size}  ${secret.updatedAt}`);
  }
  console.log(`\n${secrets.length} secret(s). Values are never shown; rotate with \`rudder cloud secrets set\`.`);
}

async function secretsRm(args: string[], options: CloudCommandOptions): Promise<void> {
  const name = args[0];
  if (!name) {
    throw new Error("Usage: rudder cloud secrets rm <NAME|~/path>");
  }
  const client = await cloudClient({ requireToken: true });
  const normalized = name.startsWith("~") || name.startsWith("/") ? toTildePath(name) : name;
  await client.request<JsonValue>(`/api/rudder/secrets/item?name=${encodeURIComponent(normalized)}`, {
    method: "DELETE",
  });
  if (options.json) {
    printJson({ ok: true, name: normalized });
  } else {
    console.log(`Removed cloud secret ${normalized}.`);
  }
}

async function collectHomeSecretFiles(): Promise<Array<{ tildePath: string; absolute: string; size: number }>> {
  const home = os.homedir();
  const out: Array<{ tildePath: string; absolute: string; size: number }> = [];
  const walk = async (target: string): Promise<void> => {
    const stat = await fsp.lstat(target).catch(() => null);
    if (!stat) {
      return;
    }
    if (stat.isSymbolicLink()) {
      return;
    }
    if (stat.isDirectory()) {
      if (!(await shouldIncludeSnapshotPath(target))) {
        return;
      }
      const entries = await fsp.readdir(target).catch(() => [] as string[]);
      for (const entry of entries) {
        await walk(path.join(target, entry));
      }
      return;
    }
    if (!stat.isFile() || !(await shouldIncludeSnapshotPath(target))) {
      return;
    }
    out.push({
      tildePath: `~/${path.relative(home, target).split(path.sep).join("/")}`,
      absolute: target,
      size: stat.size,
    });
  };
  for (const root of normalizeHomePaths([])) {
    await walk(root);
  }
  return out;
}

// One-time (re-runnable) import of the credentials that used to ride inside
// every workspace snapshot: the DEFAULT_HOME_PATHS allowlist, the macOS
// Keychain Claude token, and the captured env vars.
async function secretsSync(options: CloudCommandOptions): Promise<void> {
  const client = await cloudClient({ requireToken: true });
  const items: JsonValue[] = [];
  const skipped: Array<{ name: string; reason: string }> = [];

  for (const file of await collectHomeSecretFiles()) {
    if (file.size === 0) {
      continue;
    }
    if (file.size > MAX_SECRET_VALUE_BYTES) {
      skipped.push({ name: file.tildePath, reason: `${file.size} bytes exceeds the per-secret cap` });
      continue;
    }
    const content = await fsp.readFile(file.absolute).catch(() => null);
    if (!content) {
      skipped.push({ name: file.tildePath, reason: "unreadable" });
      continue;
    }
    items.push({
      name: file.tildePath,
      kind: "file",
      filePath: file.tildePath,
      valueBase64: content.toString("base64"),
      source: "sync",
    });
  }

  // Keychain read can pop a macOS auth dialog, so only attempt it when a
  // human is at the terminal to answer it.
  if (isTty()) {
    const keychainPayload = await readClaudeKeychainCredentials();
    if (keychainPayload) {
      items.push({
        name: "~/.claude/.credentials.json",
        kind: "file",
        filePath: "~/.claude/.credentials.json",
        valueBase64: Buffer.from(keychainPayload + "\n", "utf8").toString("base64"),
        source: "sync",
      });
    }
  }

  const capturedEnv = captureCloudEnv();
  for (const [name, value] of Object.entries(capturedEnv)) {
    items.push({
      name,
      kind: "env",
      valueBase64: Buffer.from(value, "utf8").toString("base64"),
      source: "sync",
    });
  }
  // gh keeps its token in the OS keychain, not in ~/.config/gh/hosts.yml, so
  // the synced gh config alone cannot clone a private repo in the cloud.
  if (!capturedEnv.GH_TOKEN && !capturedEnv.GITHUB_TOKEN) {
    const ghToken = await localGithubToken();
    if (ghToken) {
      items.push({ name: "GH_TOKEN", kind: "env", valueBase64: Buffer.from(ghToken, "utf8").toString("base64"), source: "sync" });
    }
  }

  if (items.length === 0) {
    throw new Error("Found nothing to sync: no allowlisted credential files or matching env vars.");
  }

  const response = await client.request<{ results?: Array<{ name: string; ok: boolean; error?: string }> }>(
    "/api/rudder/secrets/bulk",
    { method: "POST", body: { items } as JsonValue },
  );
  const results = response?.results ?? [];
  const stored = results.filter((entry) => entry.ok);
  const failed = results.filter((entry) => !entry.ok);

  if (options.json) {
    printJson({ stored: stored.length, failed, skipped } as unknown as JsonValue);
    return;
  }
  console.log(`Synced ${stored.length} secret(s) to the cloud vault:`);
  for (const entry of stored) {
    console.log(`  ${entry.name}`);
  }
  for (const entry of failed) {
    console.log(`  FAILED ${entry.name}: ${entry.error ?? "unknown error"}`);
  }
  for (const entry of skipped) {
    console.log(`  SKIPPED ${entry.name}: ${entry.reason}`);
  }
  console.log("\nNew cloud workspaces will now boot with these secrets; snapshots stop carrying local credentials.");
}

function computeWorkspaceKey(repoRoot: string): string {
  const normalized = path.resolve(repoRoot);
  return createHash("sha256").update(normalized).digest("hex").slice(0, 32);
}

// Worker placement policy: with the single-region relay in the middle of every
// attach, echo latency = RTT(client↔relay) + RTT(relay↔worker), so the worker
// belongs NEXT TO THE RELAY, not next to the user. We therefore only send a
// region when the user explicitly asked for one (env var or `rudder cloud
// region <code>`); otherwise the server places the worker in its own region.
async function explicitCloudRegion(): Promise<string | undefined> {
  const envRegion = process.env.RUDDER_CLOUD_REGION?.trim().toLowerCase();
  if (envRegion) {
    return envRegion;
  }
  const state = await loadCloudAuth().catch(() => null);
  return state?.defaultRegion?.trim().toLowerCase() || undefined;
}

async function configureRegion(args: string[], options: CloudCommandOptions): Promise<void> {
  const value = (args[0] ?? "").trim().toLowerCase();
  const state = await loadCloudAuth();
  if (!state) {
    throw new Error("Not logged in to Rudder Cloud. Run `rudder login` first.");
  }
  if (!value) {
    const current = state.defaultRegion ?? "";
    if (options.json) {
      printJson({ region: current || null });
    } else if (current) {
      console.log(`Cloud worker region override: ${current} (run \`rudder cloud region clear\` to let the server choose).`);
    } else {
      console.log("No region override set: workers are placed next to the relay for lowest typing latency.");
    }
    return;
  }
  if (value === "clear" || value === "none" || value === "auto") {
    await saveCloudAuth({ ...state, defaultRegion: undefined, updatedAt: nowIso() });
    if (options.json) {
      printJson({ ok: true, region: null });
    } else {
      console.log("Cleared region override; the server will place workers next to the relay.");
    }
    return;
  }
  if (!/^[a-z]{3,6}$/.test(value)) {
    throw new Error(`Invalid Fly region code: ${value}`);
  }
  await saveCloudAuth({ ...state, defaultRegion: value, updatedAt: nowIso() });
  if (options.json) {
    printJson({ ok: true, region: value });
  } else {
    console.log(`Cloud worker region override set to ${value}. Note: placing workers away from the relay increases typing latency.`);
  }
}

async function computeSnapshotFingerprint(
  repoRoot: string,
  _requestedHomePaths: string[],
  vaultActive = false,
): Promise<string> {
  const hash = createHash("sha256");
  // Repo state: HEAD commit + the porcelain dirty file list. Two attaches
  // from the same repo at the same commit with no edits should produce the
  // same fingerprint.
  const headCommit = await currentCommit(repoRoot).catch(() => "");
  hash.update(`repo:head:${headCommit}\n`);
  const status = await runCommand("git", ["status", "--porcelain", "-z"], {
    cwd: repoRoot,
    allowFailure: true,
  });
  if (status.code === 0) {
    hash.update(`repo:status:${status.stdout}\n`);
  }
  // With the vault active the snapshot carries no credentials, so credential
  // changes must NOT change the fingerprint: a mismatch triggers the
  // destructive destroy+recreate path server-side, and rotation already takes
  // effect on the next boot via the supervisor's vault fetch.
  if (vaultActive) {
    return hash.digest("hex").slice(0, 32);
  }
  // macOS Keychain claude credentials: hash content so re-logging in
  // invalidates the cache but a steady-state user keeps it.
  if (process.platform === "darwin") {
    const creds = await runCommand("security", ["find-generic-password", "-s", "Claude Code-credentials", "-w"], {
      allowFailure: true,
    });
    if (creds.code === 0) {
      hash.update(`keychain:claude:${createHash("sha256").update(creds.stdout).digest("hex")}\n`);
    }
  }
  // Captured env vars (excluding ones we know rotate like AWS session tokens).
  const env = captureCloudEnv();
  const unstable = new Set(["AWS_SESSION_TOKEN", "AWS_SECURITY_TOKEN"]);
  for (const key of Object.keys(env).sort()) {
    if (unstable.has(key)) continue;
    hash.update(`env:${key}:${createHash("sha256").update(env[key] ?? "").digest("hex")}\n`);
  }
  return hash.digest("hex").slice(0, 32);
}

// True when the account has vault secrets on this control plane, meaning
// snapshots should stop carrying local credentials. Any failure (old server,
// vault unconfigured, network) degrades to legacy snapshot behavior.
async function accountHasVaultSecrets(client: CloudClient): Promise<boolean> {
  if (process.env.RUDDER_CLOUD_LEGACY_SNAPSHOT_SECRETS === "1") {
    return false;
  }
  try {
    const result = await client.request<CloudSecretsListResponse>("/api/rudder/secrets", { method: "GET" });
    return (result?.secrets ?? []).length > 0;
  } catch {
    return false;
  }
}

async function workspaceAttach(args: string[], options: CloudCommandOptions): Promise<void> {
  const explicitId = args[0];
  if (explicitId) {
    if (explicitId.includes("/")) {
      await workspaceAttachByRepo(explicitId, options);
      return;
    }
    await workspaceAttachById(explicitId, options);
    return;
  }
  const repoRoot = findRepoRoot();
  const workspaceKey = computeWorkspaceKey(repoRoot);
  const repoName = path.basename(repoRoot);
  const client = await cloudClient({ requireToken: true });

  if (!options.json) {
    process.stderr.write(`Resolving cloud workspace for ${repoName}...\n`);
  }

  // Prefer an existing cloud-native (clone-based) workspace for this repo's
  // origin over uploading a snapshot of the local directory.
  const originSlug = await githubSlugFromOrigin(repoRoot);
  if (originSlug && isTty() && !options.json) {
    const cloneWorkspace = await client.request<JsonValue>(
      `/api/rudder/workspace/lookup?repo=${encodeURIComponent(originSlug)}`,
      { method: "GET" },
    ).catch(() => null);
    if (cloneWorkspace) {
      const useClone = await promptConfirm(
        `A cloud-native workspace for ${originSlug} exists. Attach it instead of uploading a local snapshot?`,
        true,
      );
      if (useClone) {
        const result = await client.request<JsonValue>("/api/rudder/workspace/create", {
          method: "POST",
          body: { repo: originSlug },
        });
        await attachToWorkspaceResult(result, options);
        return;
      }
    }
  }

  // Kick off the non-interactive work in parallel. planAgentMigration can
  // call promptConfirm for a TTY prompt, so we serialize it AFTER the
  // parallel work resolves to avoid garbled stdout during the prompt.
  const [region, vaultActive] = await Promise.all([
    explicitCloudRegion(),
    accountHasVaultSecrets(client),
  ]);
  const fingerprint = await computeSnapshotFingerprint(repoRoot, options.homePaths ?? [], vaultActive);
  const migrationPlan = await planAgentMigration(repoRoot, options);
  const mustUploadSnapshot = Boolean(migrationPlan && migrationPlan.migrated.length > 0);
  const baseBody: Record<string, JsonValue> = {
    workspaceKey,
    repoName,
    snapshotFingerprint: fingerprint,
    ...(region ? { region } : {}),
  };
  let result: JsonValue | null = null;
  if (!mustUploadSnapshot) {
    try {
      result = await client.request<JsonValue>("/api/rudder/workspace/attach", {
        method: "POST",
        body: baseBody,
      });
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      if (!/snapshot/i.test(message)) {
        throw error;
      }
      result = null;
    }
  }
  if (!result) {
    if (!options.json) {
      process.stderr.write(`Uploading workspace snapshot...\n`);
    }
    const snapshot = await createSnapshot(repoRoot, options.homePaths ?? [], {
      includeRudderState: true,
      includeCredentials: !vaultActive,
      migration: migrationPlan ? { repoName, plan: migrationPlan } : undefined,
    });
    try {
      const body: Record<string, JsonValue> = {
        ...baseBody,
        snapshot: {
          name: path.basename(snapshot.archivePath),
          contentType: "application/gzip",
          base64: await fsp.readFile(snapshot.archivePath, "base64"),
          manifest: snapshot.manifest as unknown as JsonValue,
        } as JsonValue,
      };
      if (migrationPlan && migrationPlan.migrated.length > 0) {
        body.migratedAgents = summaryAsJson(migrationPlan);
      }
      result = await client.request<JsonValue>("/api/rudder/workspace/attach", {
        method: "POST",
        body,
      });
    } finally {
      await fsp.rm(snapshot.tempDir, { recursive: true, force: true });
    }
  }
  if (migrationPlan && !options.json) {
    process.stderr.write(`${migrationSummary(migrationPlan)}\n`);
  }
  if (!result) {
    throw new Error("Workspace attach returned no result");
  }
  // The cloud workspace owns the migrated agents now. Mark their local
  // records so the next `rudder cloud` doesn't classify the same runs as
  // migratable again — without this, every attach re-offered (and re-staged)
  // agents that were already living in the cloud, forever. The dashboard's
  // migrate path does the same thing via AgentStatus::Migrated.
  if (migrationPlan) {
    await markRunsMigrated(repoRoot, migrationPlan.migrated.map((candidate) => candidate.runId));
  }
  await attachToWorkspaceResult(result, options);
}

async function markRunsMigrated(repoRoot: string, runIds: string[]): Promise<void> {
  for (const runId of runIds) {
    const recordPath = path.join(repoRoot, ".rudder", "runs", runId, "run.json");
    const record = await readJson<Record<string, JsonValue>>(recordPath);
    if (!record || record.id !== runId) {
      continue;
    }
    record.status = "migrated";
    record.updatedAt = nowIso();
    await writeJson(recordPath, record as unknown as JsonValue);
  }
}

async function planAgentMigration(
  repoRoot: string,
  options: CloudCommandOptions,
): Promise<MigrationPlan | null> {
  const candidates = await findMigrationCandidates(repoRoot);
  if (candidates.length === 0) {
    return null;
  }
  const plan = applyDefaultDecisions(candidates);
  if (options.json) {
    return plan;
  }
  if (!isTty()) {
    return plan;
  }
  console.log(migrationSummary(plan));
  if (plan.migrated.length === 0) {
    return plan;
  }
  const confirmed = await promptConfirm("Move resumable agents to cloud?", true);
  if (confirmed) {
    return plan;
  }
  return {
    candidates,
    migrated: [],
    stayedLocal: candidates.map((c) => ({ ...c, decision: "stay" as const })),
  };
}

async function attachToWorkspaceResult(result: JsonValue, options: CloudCommandOptions): Promise<void> {
  if (!result || typeof result !== "object" || Array.isArray(result)) {
    throw new Error("Unexpected workspace response from cloud");
  }
  const record = result as Record<string, JsonValue>;
  const workspaceId = typeof record.id === "string" ? record.id : undefined;
  if (!workspaceId) {
    throw new Error("Workspace response is missing id");
  }
  if (options.json) {
    printJson(record);
    return;
  }
  // The latency probe is non-interactive by design; it must not be gated on a TTY.
  if (!options.latencyProbe && (!process.stdin.isTTY || !process.stdout.isTTY)) {
    process.stderr.write(`Workspace ${workspaceId} is ready. Run \`rudder cloud workspace attach\` from a TTY to take over.\n`);
    return;
  }
  if (process.env.RUDDER_CLOUD_NO_ATTACH === "1") {
    return;
  }
  await attachWithReconnect({ kind: "workspace", id: workspaceId, label: `workspace ${workspaceId}` }, options);
}

async function workspaceAttachById(workspaceId: string, options: CloudCommandOptions): Promise<void> {
  if (options.json) {
    printJson({ id: workspaceId, attaching: true });
  } else if (!options.latencyProbe && (!process.stdin.isTTY || !process.stdout.isTTY)) {
    process.stderr.write(`Workspace ${workspaceId}: attach requires a TTY.\n`);
    return;
  }
  if (process.env.RUDDER_CLOUD_NO_ATTACH === "1") {
    return;
  }
  await attachWithReconnect({ kind: "workspace", id: workspaceId, label: `workspace ${workspaceId}` }, options);
}

async function lookupWorkspaceForRepo(
  options: CloudCommandOptions,
): Promise<Record<string, JsonValue> | null> {
  void options;
  const repoRoot = findRepoRoot();
  const workspaceKey = computeWorkspaceKey(repoRoot);
  const client = await cloudClient({ requireToken: true });
  try {
    const result = await client.request<JsonValue>(
      `/api/rudder/workspace/lookup?key=${encodeURIComponent(workspaceKey)}`,
      { method: "GET" },
    );
    if (result && typeof result === "object" && !Array.isArray(result)) {
      return result as Record<string, JsonValue>;
    }
    return null;
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    if (/not found/i.test(message) || /404/.test(message)) {
      return null;
    }
    throw error;
  }
}

async function workspaceShare(options: CloudCommandOptions): Promise<void> {
  const workspace = await lookupWorkspaceForRepo(options);
  if (!workspace) {
    if (options.json) {
      printJson({ workspace: null });
      return;
    }
    console.log("No cloud workspace exists for this repo yet. Run `rudder cloud workspace attach` to create one.");
    return;
  }
  const id = typeof workspace.id === "string" ? workspace.id : "";
  if (!id) {
    throw new Error("Workspace lookup returned no id");
  }
  if (options.json) {
    printJson({
      id,
      attachCommand: `rudder cloud workspace attach ${id}`,
      status: workspace.status ?? null,
    });
    return;
  }
  console.log("Share this workspace with a teammate by sending them:");
  console.log("");
  console.log(`  rudder cloud workspace attach ${id}`);
  console.log("");
  console.log("They must already be logged in to Rudder Cloud with their own account (run `rudder cloud login` if not).");
}

async function workspaceStatus(args: string[], options: CloudCommandOptions): Promise<void> {
  if (process.env.RUDDER_OFFLINE === "1") {
    if (options.json) {
      printJson({ offline: true, workspace: null });
    } else {
      console.log("RUDDER_OFFLINE is set; skipping cloud workspace status check.");
    }
    return;
  }
  const explicitId = args[0];
  const workspace = await (explicitId
    ? lookupWorkspaceById(explicitId)
    : lookupWorkspaceForRepo(options)).catch((error) => {
    if (!options.json) {
      console.warn(`Could not reach Rudder Cloud: ${error instanceof Error ? error.message : String(error)}`);
    }
    return null;
  });
  if (!workspace) {
    if (options.json) {
      printJson({ workspace: null });
    } else {
      console.log("No cloud workspace for this repo.");
    }
    return;
  }
  const id = typeof workspace.id === "string" ? workspace.id : "";
  const status = typeof workspace.status === "string" ? workspace.status : "unknown";
  const clientCount = typeof workspace.clientCount === "number" ? workspace.clientCount : 0;
  const lastActivityAt = typeof workspace.lastActivityAt === "string" ? workspace.lastActivityAt : undefined;
  const idleMinutes = computeIdleMinutes(lastActivityAt);
  // Prefer the server's worker-reported busy count (a fact: how many rudder
  // runs are actively working in that machine). Fall back to the old activity
  // heuristic only for control planes that predate the field.
  const serverActiveAgents = typeof workspace.activeAgents === "number" ? workspace.activeAgents : null;
  const activeAgents = serverActiveAgents !== null
    ? serverActiveAgents > 0
    : clientCount > 0 || (idleMinutes !== null && idleMinutes < 5);
  if (options.json) {
    printJson({
      id,
      status,
      clientCount,
      lastActivityAt: lastActivityAt ?? null,
      idleMinutes,
      activeAgents,
      activeAgentCount: serverActiveAgents,
      repoName: typeof workspace.repoName === "string" ? workspace.repoName : null,
    });
    return;
  }
  const idlePart = idleMinutes !== null ? `  idle ${idleMinutes}m` : "";
  console.log(`workspace ${id}  ${status}  clients=${clientCount}${idlePart}`);
  if (status === "failed" && typeof workspace.failureReason === "string" && workspace.failureReason) {
    console.log(`why: ${workspace.failureReason}`);
  }
  if (serverActiveAgents !== null) {
    if (serverActiveAgents > 0) {
      console.log(`${serverActiveAgents} agent(s) actively running in the cloud.`);
    } else {
      console.log("No agents running in the cloud right now.");
    }
  } else if (activeAgents) {
    console.log("Active agents likely running.");
  } else {
    console.log("No recent activity.");
  }
}

async function lookupWorkspaceById(id: string): Promise<Record<string, JsonValue> | null> {
  const client = await cloudClient({ requireToken: true });
  const result = await client.request<JsonValue>("/api/rudder/workspace", { method: "GET" });
  const match = collectionFromResult(result, "workspaces").find((item) =>
    item && typeof item === "object" && !Array.isArray(item)
      && (item as Record<string, JsonValue>).id === id
  );
  return match && typeof match === "object" && !Array.isArray(match)
    ? match as Record<string, JsonValue>
    : null;
}

function computeIdleMinutes(lastActivityAt: string | undefined): number | null {
  if (!lastActivityAt) {
    return null;
  }
  const ms = Date.parse(lastActivityAt);
  if (!Number.isFinite(ms)) {
    return null;
  }
  const diff = Date.now() - ms;
  if (diff < 0) {
    return 0;
  }
  return Math.floor(diff / 60_000);
}

async function workspaceMutate(action: "pause" | "resume" | "stop", args: string[], options: CloudCommandOptions): Promise<void> {
  const id = args[0];
  if (!id) {
    throw new Error(`Usage: rudder cloud workspace ${action} <id>`);
  }
  const client = await cloudClient({ requireToken: true });
  const result = await client.request<JsonValue>(`/api/rudder/workspace/${encodeURIComponent(id)}/${action}`, {
    method: "POST",
    body: {} as JsonValue,
  });
  if (options.json) {
    printJson(result);
    return;
  }
  const record = result && typeof result === "object" && !Array.isArray(result)
    ? result as Record<string, JsonValue>
    : {};
  const status = typeof record.status === "string" ? record.status : action;
  console.log(`Workspace ${id} ${status}.`);
}

/**
 * Strip `--yes`/`-y` out of a subcommand's args. Destructive commands confirm
 * unless one of these (or the global `--force`) is given.
 */
function takeYesFlag(args: string[], options: CloudCommandOptions): { args: string[]; yes: boolean } {
  const rest = args.filter((arg) => arg !== "--yes" && arg !== "-y");
  return { args: rest, yes: Boolean(options.yes) || rest.length !== args.length };
}

/**
 * Ask before destroying something in the cloud. Never assumes yes: without a
 * terminal to ask on, the caller must pass --yes explicitly.
 */
async function confirmCloudDeletion(what: string, yes: boolean): Promise<boolean> {
  if (yes) {
    return true;
  }
  if (!process.stdin.isTTY || !process.stdout.isTTY) {
    throw new Error(`Refusing to delete ${what} without confirmation; pass --yes to confirm.`);
  }
  const readline = await import("node:readline/promises");
  const rl = readline.createInterface({ input: process.stdin, output: process.stdout });
  try {
    const answer = await rl.question(`Delete ${what}? This destroys its cloud machine and data and cannot be undone. [y/N] `);
    return /^y(es)?$/i.test(answer.trim());
  } finally {
    rl.close();
  }
}

function describeDeletion(kind: string, id: string, result: JsonValue): string {
  const record = result && typeof result === "object" && !Array.isArray(result)
    ? result as Record<string, JsonValue>
    : {};
  if (record.status === "deleted") {
    return `Deleted ${kind} ${id}.`;
  }
  const reason = typeof record.error === "string" ? ` (${record.error})` : "";
  return `Deleting ${kind} ${id}: Rudder Cloud is finishing the teardown and will retry until it is gone${reason}.`;
}

async function deleteSail(args: string[], options: CloudCommandOptions): Promise<void> {
  const { args: rest, yes } = takeYesFlag(args, options);
  const id = rest[0];
  if (!id) {
    throw new Error("Usage: rudder cloud delete <sail-id> [--yes]");
  }
  if (!(await confirmCloudDeletion(`cloud sail ${id}`, yes))) {
    console.log("Kept it; nothing was deleted.");
    return;
  }
  const client = await cloudClient({ requireToken: true });
  const result = await client.request<JsonValue>(`/api/rudder/sail/${encodeURIComponent(id)}/delete`, {
    method: "POST",
    body: {} as JsonValue,
  });
  if (options.json) {
    printJson(result);
    return;
  }
  console.log(describeDeletion("sail", id, result));
}

async function workspaceDelete(args: string[], options: CloudCommandOptions): Promise<void> {
  const { args: rest, yes } = takeYesFlag(args, options);
  // No id: this repo's workspace, the same one `workspace status` reports.
  const workspace = rest[0] ? await lookupWorkspaceById(rest[0]) : await lookupWorkspaceForRepo(options);
  const id = workspace && typeof workspace.id === "string" ? workspace.id : rest[0];
  if (!workspace || !id) {
    throw new Error(rest[0] ? `No cloud workspace ${rest[0]}.` : "No cloud workspace for this repo.");
  }
  const repo = typeof workspace.repoName === "string" ? ` (${workspace.repoName})` : "";
  const agents = typeof workspace.activeAgents === "number" && workspace.activeAgents > 0
    ? `, which has ${workspace.activeAgents} agent(s) running`
    : "";
  if (!(await confirmCloudDeletion(`cloud workspace ${id}${repo}${agents}`, yes))) {
    console.log("Kept it; nothing was deleted.");
    return;
  }
  const client = await cloudClient({ requireToken: true });
  const result = await client.request<JsonValue>(`/api/rudder/workspace/${encodeURIComponent(id)}/delete`, {
    method: "POST",
    body: {} as JsonValue,
  });
  if (options.json) {
    printJson(result);
    return;
  }
  console.log(describeDeletion("workspace", id, result));
}

async function workspaceList(options: CloudCommandOptions): Promise<void> {
  const client = await cloudClient({ requireToken: true });
  const result = await client.request<JsonValue>("/api/rudder/workspace", { method: "GET" });
  await printResult(result, options);
}

async function attach(args: string[], options: CloudCommandOptions): Promise<void> {
  const sailId = args[0];
  if (!sailId) {
    // In a repo, `rudder cloud attach` is coming back to it: open it on the
    // home machine (waking it if needed) and attach, without the first-run
    // offers `rudder cloud` makes.
    await openOnHome(false, { ...options, reattach: true });
    return;
  }
  // A project on the home machine, by name: the way back to one made there
  // (the phone's "new project" has no GitHub origin for a checkout to name).
  if (await attachHomeProject(sailId, options)) {
    return;
  }
  const client = await cloudClient({ requireToken: true });
  const listed = await client.request<{ sails?: { id?: string }[] }>("/api/rudder/sail", { method: "GET" }).catch(() => null);
  if (Array.isArray(listed?.sails) && !listed.sails.some((sail) => sail?.id === sailId)) {
    throw new Error(`no cloud worker or home project named ${sailId}. \`rudder cloud home\` lists the projects on your home machine.`);
  }
  await attachWithReconnect({ kind: "sail", id: sailId, label: sailId }, options);
}

async function attachHomeProject(name: string, options: CloudCommandOptions): Promise<boolean> {
  const client = await cloudClient({ requireToken: true });
  let projects: Record<string, JsonValue>[] = [];
  try {
    const result = await client.request<{ projects?: Record<string, JsonValue>[] }>("/api/rudder/home", { method: "GET" });
    projects = result.projects ?? [];
  } catch {
    return false;
  }
  const wanted = name.toLowerCase();
  const project = projects.find((p) => String(p.name ?? "").toLowerCase() === wanted || String(p.repo ?? "").toLowerCase() === wanted)
    ?? projects.find((p) => String(p.repo ?? "").toLowerCase().split("/")[1] === wanted);
  if (!project) {
    return false;
  }
  const open = () => client.request<HomeOpenResponse>("/api/rudder/home/open", { method: "POST", body: { project: String(project.name) } });
  const result = await open();
  const workspaceId = typeof result.workspace?.id === "string" ? result.workspace.id : "";
  if (!workspaceId) {
    throw new Error("the control plane did not return a workspace for this project");
  }
  if (options.json) {
    printJson(result as unknown as JsonValue);
    return true;
  }
  if (!process.stdin.isTTY || !process.stdout.isTTY) {
    process.stderr.write(`${project.name} is open on the home machine (workspace ${workspaceId}). Run \`rudder cloud attach ${project.name}\` from a terminal to attach.\n`);
    return true;
  }
  process.stderr.write(`Opening ${project.name} on your home machine...\n`);
  await attachWithReconnect({ kind: "workspace", id: workspaceId, label: `${project.name} on home` }, options, async () => {
    await open();
  });
  return true;
}

type AttachTarget = {
  kind: "sail" | "workspace";
  id: string;
  label: string;
};

const ATTACH_PING_MS = 5_000;
const ATTACH_PONG_TIMEOUT_MS = 20_000;
const ATTACH_SLEEP_GAP_MS = 15_000;
// Retry at once (a blip, a deploy), then back off; never more than 5s apart,
// so coming back to a reachable cloud never waits long.
const ATTACH_RETRY_DELAYS_MS = [250, 1_000, 2_000, 3_000, 5_000];
// About half an hour of failed tries while awake (asleep, timers do not run).
const ATTACH_MAX_RETRIES = 360;
const ATTACH_REOPEN_TIMEOUT_MS = 20_000;
const ATTACH_HANDSHAKE_TIMEOUT_MS = 20_000;

// "dropped": the connection ended without the user leaving (the control plane
// redeployed, the home machine restarted). Callers reconnect.
type AttachResult = "exited" | "failed" | "dropped";

// The remote TUI pushes the kitty keyboard enhancement flags once, in its own
// setup_terminal(), and the worker only keeps the TAIL of its output — so by the
// time an attach connects, that push is long gone and this terminal never sees
// it. Same class of bug as the mouse modes below. Without the protocol, macOS
// terminals report Option+h as the character it TYPES (˙ on a US layout) rather
// than Alt+h, so ⌥h stops hiding panes, and Alt+[ / Alt+] cannot be encoded at
// all. AttachSession.hold pushes exactly what the remote asked for:
// DISAMBIGUATE_ESCAPE_CODES (1) | REPORT_ALTERNATE_KEYS (4). Terminals without
// the protocol ignore it.

// Stay attached through a dropped link, a control-plane deploy or a machine
// restart. One AttachSession holds the terminal the whole time, so a drop is a
// badge in the corner and a reconnect, never a trip back to the shell.
async function attachWithReconnect(
  target: AttachTarget,
  options: CloudCommandOptions,
  reopen?: () => Promise<void>,
): Promise<AttachResult> {
  if (options.latencyProbe) {
    return await runAttach(target, options);
  }
  const stdin = process.stdin;
  const stdout = process.stdout;
  const session = new AttachSession(target.label, stdin, stdout, {
    interactive: Boolean(stdin.isTTY && stdout.isTTY) && !options.json,
    predict: process.env.RUDDER_PREDICT !== "0",
  });
  let result: AttachResult = "failed";
  let gaveUp = false;
  // Closing the tab (SIGHUP), a kill (SIGTERM) or a crash must still give the
  // terminal back, or the shell is left reporting every key and the mouse.
  const releaseOnExit = () => session.release();
  const releaseAndExit = (code: number) => () => {
    session.release();
    process.exit(code);
  };
  const onHup = releaseAndExit(129);
  const onTerm = releaseAndExit(143);
  process.on("exit", releaseOnExit);
  process.once("SIGHUP", onHup);
  process.once("SIGTERM", onTerm);
  try {
    result = await runAttach(target, { ...options, quietBanner: false }, session);
    let attempts = 0;
    while (result === "dropped" && !session.hasLeft) {
      attempts += 1;
      if (attempts > ATTACH_MAX_RETRIES) {
        gaveUp = true;
        break;
      }
      session.noteAttempt(attempts);
      const delay = ATTACH_RETRY_DELAYS_MS[Math.min(attempts, ATTACH_RETRY_DELAYS_MS.length) - 1] ?? 5_000;
      if (await session.wait(delay)) {
        break;
      }
      try {
        if (reopen) {
          // Wakes the machine if it was what went away. Bounded: a hung
          // request must not stall the retries, and Ctrl+C twice still leaves.
          await raceLeave(session, withTimeout(reopen(), ATTACH_REOPEN_TIMEOUT_MS));
        }
        if (session.hasLeft) {
          break;
        }
        result = await runAttach(target, { ...options, quietBanner: true }, session);
        attempts = 0;
      } catch {
        result = "dropped";
      }
    }
    if (session.hasLeft && result === "dropped") {
      result = "exited";
    }
  } finally {
    session.release();
    process.off("exit", releaseOnExit);
    process.off("SIGHUP", onHup);
    process.off("SIGTERM", onTerm);
  }
  if (gaveUp) {
    process.stderr.write(`\nCould not reach Rudder Cloud for a while. Your agents are still running there; run \`rudder cloud\` to come back.\n`);
  } else if (session.hasLeft && result === "exited") {
    process.stderr.write(`\nLeft ${target.label}. Your agents keep running; run \`rudder cloud\` to come back.\n`);
  }
  return result;
}

function withTimeout<T>(promise: Promise<T>, ms: number): Promise<T> {
  return new Promise<T>((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error("timed out")), ms);
    timer.unref?.();
    promise.then(
      (value) => { clearTimeout(timer); resolve(value); },
      (error: unknown) => { clearTimeout(timer); reject(error); },
    );
  });
}

function raceLeave<T>(session: AttachSession, promise: Promise<T>): Promise<T | undefined> {
  return new Promise<T | undefined>((resolve, reject) => {
    const off = session.onLeave(() => resolve(undefined));
    promise.then(
      (value) => { off(); resolve(value); },
      (error: unknown) => { off(); reject(error); },
    );
  });
}

// One pasted file up to the home machine, through the phone's upload route.
async function uploadPastedFile(baseUrl: string, token: string, workspaceId: string, file: string, name: string): Promise<string> {
  const bytes = await fsp.readFile(file);
  const url = `${baseUrl.replace(/\/$/, "")}/api/app/upload?workspace=${encodeURIComponent(workspaceId)}&name=${encodeURIComponent(name)}`;
  const response = await fetch(url, {
    method: "POST",
    headers: { authorization: `Bearer ${token}`, "content-type": "application/octet-stream" },
    body: bytes,
    signal: AbortSignal.timeout(90_000),
  });
  const body = await response.json().catch(() => ({})) as { path?: string; error?: string };
  if (!response.ok || typeof body.path !== "string") {
    throw new Error(body.error || `HTTP ${response.status}`);
  }
  return body.path;
}

async function runAttach(
  target: AttachTarget,
  options: CloudCommandOptions,
  sharedSession?: AttachSession,
): Promise<AttachResult> {
  const client = await cloudClient({ requireToken: true });
  const baseUrl = client.baseUrl;
  const state = await loadCloudAuth();
  const envToken = process.env.RUDDER_CLOUD_TOKEN?.trim();
  const token = envToken || (state?.cloudUrl === baseUrl ? state.token : undefined);
  if (!token) {
    throw new Error("Not logged in to Rudder Cloud. Run `rudder login` first.");
  }

  const stdin = process.stdin;
  const stdout = process.stdout;
  const probeMode = Boolean(options.latencyProbe);
  const isInteractive = Boolean(stdin.isTTY && stdout.isTTY) && !probeMode;
  // A shared session outlives this connection (attachWithReconnect); an own
  // one is released with it.
  const session = sharedSession ?? new AttachSession(target.label, stdin, stdout, {
    interactive: isInteractive && !options.json,
    predict: process.env.RUDDER_PREDICT !== "0",
  });
  const reconnects = Boolean(sharedSession);
  // The dashboard is already on the screen (a reconnect, not a first attach).
  const resuming = session.connections > 0 && session.drewFrame;
  // The relay hands control back to this same terminal when it returns,
  // instead of keeping its dead socket in charge until a ping times out.
  const wsUrl = baseUrl.replace(/^http/, "ws").replace(/\/$/, "")
    + `/api/rudder/${target.kind}/${encodeURIComponent(target.id)}/attach?client=${encodeURIComponent(session.clientId)}`;

  return await new Promise<AttachResult>((resolve, reject) => {
    const socket = new WebSocket(wsUrl, {
      headers: { authorization: `Bearer ${token}` },
      // Do NOT negotiate permessage-deflate on the attach client. Keystrokes and
      // echoes are tiny (uncompressed either way), the remote dashboard writes
      // cell-diffs, and with deflate enabled every redraw over 1KB pays a zlib
      // round on BOTH sides — on the relay's event loop that also head-of-line
      // blocks the keystroke frames queued behind a redraw burst, which is
      // exactly "typing feels slow while the agent is streaming". Raw frames
      // are bounded (~100KB worst-case repaint), so the bandwidth cost of
      // skipping compression is negligible next to the latency it removes.
      perMessageDeflate: false,
      // A connect into a dead network can otherwise sit for minutes.
      handshakeTimeout: ATTACH_HANDSHAKE_TIMEOUT_MS,
    });
    socket.binaryType = "nodebuffer";
    // Every deliberate close goes through socket.close() from this side; any
    // other close is the far end going away.
    let closedByUs = false;
    const closeSocket = socket.close.bind(socket);
    socket.close = ((code?: number, reason?: string | Buffer) => {
      closedByUs = true;
      closeSocket(code, reason);
    }) as typeof socket.close;
    let opened = false;
    let cleaned = false;
    let firstFrameRendered = resuming;
    // What the relay last said about the machine end: "down" only once it has
    // said so, so a key typed before the first status still goes through.
    let workerState: "unknown" | "up" | "down" = "unknown";
    let hasControl = true;
    let result: AttachResult = "failed";

    const splashAllowed = isInteractive && !options.json && !options.quietBanner && !resuming;
    const splash = splashAllowed ? new AttachSplash(stdout, target.label) : null;
    // Predictive local echo (src/predict.ts): typed characters appear at the
    // caret at once and the cloud's frame confirms them. RUDDER_PREDICT=0
    // turns the drawing off; the model stays, the reconnect badge uses it.
    const predictor = session.predictor;

    const sendResize = () => {
      if (socket.readyState !== WebSocket.OPEN) {
        return;
      }
      const cols = stdout.columns ?? 120;
      const rows = stdout.rows ?? 32;
      predictor?.resize(cols, rows);
      socket.send(JSON.stringify({ type: "resize", cols, rows }));
    };

    // Ask the dashboard to draw every cell again. The replay the relay sends
    // on attach is only the tail of its output; this makes the screen exact.
    const requestRepaint = () => {
      if (socket.readyState === WebSocket.OPEN && !probeMode) {
        socket.send(JSON.stringify({ type: "repaint" }));
      }
    };

    const finish = (value: AttachResult, message?: string) => {
      cleanup(false);
      if (message) {
        process.stderr.write(message);
      }
      resolve(value);
    };

    // Files and clipboard images from this computer reach the cloud machine
    // as uploads (src/paste-bridge.ts); only the home's repo rows take them.
    const paste = new PasteBridge({
      send: (data) => {
        if (socket.readyState === WebSocket.OPEN) {
          socket.send(data, { binary: true });
          predictor?.onInput(data);
        }
      },
      upload: (file, name) => uploadPastedFile(baseUrl, token, target.id, file, name),
      readClipboard: target.kind === "workspace" && isInteractive ? readLocalClipboard : undefined,
      status: (text) => (text ? session.showStatus(text) : session.clearStatus()),
    });
    if (target.kind !== "workspace") {
      paste.disabled = true;
    }

    let lastCtrlC = 0;
    // This terminal's light/dark colors, for the agents on the machine to
    // theme themselves by (src/terminal-theme.ts). Sent on every connection.
    const sendTheme = (colors: { fg: string | null; bg: string | null }) => {
      if (socket.readyState === WebSocket.OPEN && hasControl && colors.bg && !probeMode) {
        socket.send(JSON.stringify({ type: "theme", fg: colors.fg, bg: colors.bg }));
      }
    };
    const offTheme = session.onTheme(sendTheme);

    const onStdin = (chunk: Buffer | string) => {
      const buffer = session.filterInput(typeof chunk === "string" ? Buffer.from(chunk, "utf8") : chunk);
      if (socket.readyState !== WebSocket.OPEN || buffer.length === 0) {
        return;
      }
      const hasCtrlC = containsCtrlC(buffer);
      // While the loading splash is up (no remote frame has rendered yet),
      // Ctrl+C should cancel the local attach instead of being forwarded to
      // a remote dashboard the user can't see.
      if (hasCtrlC && !firstFrameRendered) {
        try { socket.close(1000, "client-cancel"); } catch { /* ignore */ }
        finish("exited", "\nCancelled.\n");
        return;
      }
      // An observer must always be able to leave without sending an interrupt
      // into the controller's shared PTY.
      if (hasCtrlC && !hasControl) {
        try { socket.close(1000, "read-only-client-cancel"); } catch { /* ignore */ }
        finish("exited", "\nDetached read-only cloud attachment.\n");
        return;
      }
      if (!hasControl) {
        session.showStatus("Read-only · another terminal has control");
        return;
      }
      // The machine end is away (its dashboard restarting, the machine
      // waking). Forwarding Ctrl+C would kill the next dashboard as soon as it
      // is back, so it never goes; pressed twice it leaves this attach.
      if (hasCtrlC && workerState === "down") {
        if (session.ctrlCToLeave()) {
          try { socket.close(1000, "client-cancel-disconnected"); } catch { /* ignore */ }
          finish("exited");
        }
        return;
      }
      // After handoff: forward Ctrl+C to the remote so Claude/codex can be
      // cancelled, but if the user mashes Ctrl+C twice within 2 seconds we
      // take it as "the remote is unresponsive; get me out".
      if (hasCtrlC && firstFrameRendered) {
        const now = Date.now();
        if (now - lastCtrlC < 2000) {
          try { socket.close(1000, "client-force-quit"); } catch { /* ignore */ }
          finish("exited", "\nForce-exiting local attach (press Ctrl+C again to re-enter).\n");
          return;
        }
        lastCtrlC = now;
      }
      paste.forward(buffer);
    };

    const onResize = () => {
      if (hasControl) {
        sendResize();
      }
      splash?.redraw();
      session.noteResize();
    };

    // Liveness. A laptop that slept leaves this socket half-open: nothing
    // errors, the screen just freezes, and the relay has meanwhile dropped us
    // (or handed control to the next terminal). Ping the relay, and treat a
    // missed pong or a timer that fired far too late (the machine was asleep)
    // as a dropped connection, which attachWithReconnect reopens.
    let lastHeard = Date.now();
    let lastTick = Date.now();
    let pingSentAt = 0;
    socket.on("pong", () => {
      lastHeard = Date.now();
      if (pingSentAt) {
        predictor?.noteRtt(Date.now() - pingSentAt);
        pingSentAt = 0;
      }
    });
    socket.on("message", () => { lastHeard = Date.now(); });
    const liveness = setInterval(() => {
      const now = Date.now();
      const slept = now - lastTick > ATTACH_SLEEP_GAP_MS;
      lastTick = now;
      if (socket.readyState !== WebSocket.OPEN) {
        return;
      }
      if (slept || now - lastHeard > ATTACH_PONG_TIMEOUT_MS) {
        socket.terminate();
        return;
      }
      try {
        socket.ping();
        pingSentAt = Date.now();
      } catch {
        socket.terminate();
      }
    }, ATTACH_PING_MS);
    liveness.unref?.();

    // Leaving (Ctrl+C twice) while this connection is still being made.
    const offLeave = session.onLeave(() => {
      if (!opened && !cleaned) {
        try { socket.terminate(); } catch { /* ignore */ }
        cleanup(false);
        resolve("exited");
      }
    });

    // keepTerminal: the link dropped (or a reconnect could not open) and the
    // caller reconnects, so the screen, raw mode and the keys stay ours.
    const cleanup = (keepTerminal: boolean) => {
      if (cleaned) {
        return;
      }
      cleaned = true;
      offLeave();
      clearInterval(liveness);
      stdin.off("data", onStdin);
      offTheme();
      stdout.off("resize", onResize);
      process.off("SIGINT", onSigint);
      splash?.stop();
      if (keepTerminal) {
        session.enterGap();
      } else {
        session.release();
      }
    };

    const onSigint = () => {
      // Belt-and-suspenders: if raw mode failed for any reason, Node still
      // sees SIGINT. Close cleanly so the user never gets a frozen terminal.
      try { socket.close(1000, "sigint"); } catch { /* ignore */ }
      cleanup(false);
      process.stderr.write("\nReceived SIGINT, closing cloud attach.\n");
      if (!opened) {
        reject(new Error("Cloud attach cancelled"));
      } else {
        resolve("exited");
      }
    };
    process.once("SIGINT", onSigint);

    // --latency-probe state: transport samples resolve on a `probe-reply`
    // control frame from the worker (pure WS relay round trip); echo samples
    // resolve on the next binary frame after sending a printable keystroke
    // (full pipeline including the remote TUI render).
    const probeReplies = new Map<number, (at: number) => void>();
    let probeEchoWaiter: ((at: number) => void) | null = null;
    let probeSeq = 0;

    const runLatencyProbe = async (): Promise<void> => {
      const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));
      const SAMPLES = 20;
      const transport: number[] = [];
      const echo: number[] = [];
      // Let the remote dashboard finish its initial redraw burst so spinner
      // frames don't get mistaken for keystroke echoes.
      await sleep(750);
      for (let i = 0; i < SAMPLES; i += 1) {
        if (socket.readyState !== WebSocket.OPEN) break;
        const id = ++probeSeq;
        const sentAt = performance.now();
        const repliedAt = await new Promise<number | null>((resolveReply) => {
          const timer = setTimeout(() => {
            probeReplies.delete(id);
            resolveReply(null);
          }, 2000);
          probeReplies.set(id, (at) => {
            clearTimeout(timer);
            probeReplies.delete(id);
            resolveReply(at);
          });
          socket.send(JSON.stringify({ type: "probe", id }));
        });
        if (repliedAt !== null) transport.push(repliedAt - sentAt);
        await sleep(100);
      }
      for (let i = 0; i < SAMPLES; i += 1) {
        if (socket.readyState !== WebSocket.OPEN) break;
        const sentAt = performance.now();
        const echoedAt = await new Promise<number | null>((resolveFrame) => {
          const timer = setTimeout(() => {
            probeEchoWaiter = null;
            resolveFrame(null);
          }, 2000);
          probeEchoWaiter = (at) => {
            clearTimeout(timer);
            probeEchoWaiter = null;
            resolveFrame(at);
          };
          socket.send(Buffer.from("a"), { binary: true });
        });
        if (echoedAt !== null) echo.push(echoedAt - sentAt);
        // Undo the probe keystroke so the remote input box is left untouched.
        socket.send(Buffer.from("\x7f"), { binary: true });
        await sleep(250);
      }
      const stats = (values: number[]): string => {
        if (values.length === 0) return "no samples (timed out)";
        const sorted = [...values].sort((a, b) => a - b);
        const at = (q: number) => sorted[Math.min(sorted.length - 1, Math.floor(q * sorted.length))] ?? 0;
        const max = sorted[sorted.length - 1] ?? 0;
        return `p50 ${at(0.5).toFixed(1)}ms  p95 ${at(0.95).toFixed(1)}ms  max ${max.toFixed(1)}ms  (${sorted.length}/${SAMPLES} samples)`;
      };
      const report = [
        "",
        `Latency probe · ${target.label}`,
        `  transport RTT  ${stats(transport)}`,
        `  keystroke echo ${stats(echo)}`,
        "",
      ].join("\n");
      if (options.json) {
        process.stdout.write(`${JSON.stringify({ target: target.label, transportMs: transport, echoMs: echo })}\n`);
      } else {
        process.stderr.write(report);
      }
      result = "exited";
      try { socket.close(1000, "probe-done"); } catch { /* ignore */ }
    };

    socket.on("open", () => {
      opened = true;
      session.connections += 1;
      // From here this connection reads the keys; the gap hands them over.
      session.exitGap();
      // Measure the round trip at once: predictive echo waits on it.
      try {
        socket.ping();
        pingSentAt = Date.now();
      } catch {
        // the liveness timer retries
      }
      if (probeMode) {
        // An idle dashboard sends nothing; a resize makes it repaint, which is
        // the first frame the probe waits for.
        socket.send(JSON.stringify({ type: "resize", cols: 119, rows: 32 }));
        setTimeout(() => socket.send(JSON.stringify({ type: "resize", cols: 120, rows: 32 })), 200);
      }
      // Disable Nagle on the underlying TCP socket so single keystrokes don't
      // sit in the send buffer waiting for piggyback ACKs (~40ms savings per
      // keypress on the WAN).
      try {
        const underlying = (socket as unknown as { _socket?: { setNoDelay?: (v: boolean) => void } })._socket;
        underlying?.setNoDelay?.(true);
      } catch {
        // ignore
      }
      // Label this terminal tab so the user can find the right rudder cloud
      // session at a glance instead of squinting at a row of "ghostty" tabs.
      if (isInteractive) {
        try {
          stdout.write(`\x1b]0;Rudder cloud: ${target.label}\x07`);
        } catch {
          // ignore
        }
      }
      // The alternate screen, raw keys, local mouse capture (the remote's own
      // enable sequences arrive as output, which the local terminal never
      // treats as mode switches) and the kitty keyboard flags.
      if (isInteractive) {
        session.hold();
      }
      if (splash) {
        splash.start();
        splash.setStatus(`Booting cloud workspace · ${target.label}`);
      } else if (!options.json && !options.quietBanner && !resuming) {
        const tail = isInteractive ? " (Ctrl+C sends to remote; close this pane to detach)" : "";
        process.stderr.write(`Attached to ${target.label}${tail}\n`);
      }
      sendResize();
      // Typed while the link was down: sent first, in order.
      const queued = session.takeQueued();
      if (queued) {
        socket.send(queued, { binary: true });
      }
      requestRepaint();
      if (session.theme?.known) {
        sendTheme(session.theme.colors);
      }
      if (!probeMode) {
        stdin.resume();
        stdin.on("data", onStdin);
      } else {
        // A worker that never boots would otherwise hang the probe forever:
        // there is no TTY and no human to Ctrl+C it.
        const firstFrameDeadline = setTimeout(() => {
          if (!firstFrameRendered) {
            process.stderr.write(
              "Latency probe timed out waiting for the first remote frame (worker did not boot?).\n",
            );
            result = "failed";
            try { socket.close(1000, "probe-timeout"); } catch { /* ignore */ }
          }
        }, 120_000);
        firstFrameDeadline.unref?.();
      }
      stdout.on("resize", onResize);
    });

    socket.on("message", (data, isBinary) => {
      if (isBinary && Buffer.isBuffer(data)) {
        if (probeMode) {
          // Frames are timing signals here, not screen content: the first one
          // marks the dashboard as live (start probing), later ones resolve a
          // pending keystroke-echo sample.
          if (!firstFrameRendered) {
            firstFrameRendered = true;
            void runLatencyProbe().catch((err: unknown) => {
              process.stderr.write(`Latency probe failed: ${err instanceof Error ? err.message : String(err)}\n`);
              try { socket.close(1000, "probe-failed"); } catch { /* ignore */ }
            });
            return;
          }
          probeEchoWaiter?.(performance.now());
          return;
        }
        if (!session.drewFrame) {
          session.drewFrame = true;
          // A link that dropped while the loading screen was up leaves its
          // text behind; the splash wipes its own on handoff.
          if (!splash && isInteractive) {
            stdout.write("\x1b[2J\x1b[H");
          }
        }
        if (!firstFrameRendered) {
          firstFrameRendered = true;
          splash?.handoff();
          // Re-send resize right after handoff and once more on a small
          // delay; some terminals don't surface accurate columns/rows
          // until after alt-screen exits, so the initial resize at WS
          // open can race.
          sendResize();
          setTimeout(sendResize, 150);
        }
        stdout.write(predictor ? predictor.decorate(data) : data);
        predictor?.onServerData(data);
        return;
      }
      const text = Buffer.isBuffer(data)
        ? data.toString("utf8")
        : Array.isArray(data)
          ? Buffer.concat(data).toString("utf8")
          : Buffer.from(data as ArrayBuffer).toString("utf8");
      handleControlText(text);
    });

    socket.on("close", (code, reason) => {
      const dropped = opened && !closedByUs && code !== 4410 && code !== 1000;
      // A reconnect that could not open is part of the same gap.
      cleanup(reconnects && session.connections > 0 && (dropped || !opened));
      if (!opened) {
        const reasonText = reason && reason.length ? reason.toString("utf8") : "";
        reject(new Error(`Cloud attach failed (code ${code}${reasonText ? `: ${reasonText}` : ""})`));
        return;
      }
      // 4410: the workspace was deleted; nothing to come back to.
      resolve(dropped ? "dropped" : result);
    });

    socket.on("error", (err) => {
      if (!opened) {
        cleanup(reconnects && session.connections > 0);
        reject(new Error(`Cloud attach failed: ${err.message}`));
      }
    });

    function handleControlText(text: string): void {
      let payload: unknown;
      try {
        payload = JSON.parse(text);
      } catch {
        stdout.write(text);
        return;
      }
      if (!payload || typeof payload !== "object") {
        return;
      }
      const message = payload as { type?: string; state?: string; code?: number; id?: number; control?: string };
      if (message.type === "probe-reply") {
        if (typeof message.id === "number") {
          probeReplies.get(message.id)?.(performance.now());
        }
        return;
      }
      if (message.type === "exit") {
        result = message.code === 0 ? "exited" : "failed";
        if (typeof process.exitCode !== "number" && message.code !== undefined) {
          process.exitCode = message.code;
        }
        return;
      }
      if (message.type !== "status") {
        return;
      }
      const wasDown = workerState === "down";
      if (message.state === "worker-connected") {
        workerState = "up";
      } else if (message.state === "worker-disconnected" || message.state === "worker-waiting") {
        workerState = "down";
      }
      if (message.control === "read-only" || message.state === "read-only") {
        hasControl = false;
      } else if (message.control === "active" || message.state === "controller-promoted") {
        const promoted = !hasControl;
        hasControl = true;
        if (promoted || message.state === "controller-promoted") {
          sendResize();
        }
      }
      // If the worker dies before we've ever seen a remote frame, the
      // session is effectively dead. Don't sit on a splash spinner pretending
      // it'll come back; bail.
      if (message.state === "worker-disconnected" && !firstFrameRendered) {
        try { socket.close(1000, "worker-gone"); } catch { /* ignore */ }
        cleanup(false);
        process.stderr.write("\nCloud worker exited before the dashboard started.\n");
        resolve("failed");
        return;
      }
      if (splash && !firstFrameRendered) {
        if (message.state === "worker-waiting") {
          splash.setStatus(`Waiting for cloud worker · ${target.label}`);
        } else if (message.state === "worker-connected") {
          splash.setStatus(`Cloud worker connected · ${target.label}`);
          sendResize();
          setTimeout(sendResize, 150);
        } else if (message.state === "input-buffered") {
          splash.setStatus(`Reconnecting cloud input · ${target.label}`);
        }
        return;
      }
      if (options.json || probeMode) {
        return;
      }
      // Past the splash nothing is written into the dashboard's picture: the
      // state goes in the corner badge and comes off when it is over.
      if (!hasControl) {
        session.showStatus("Read-only · another terminal has control");
      } else if (workerState === "down") {
        session.showStatus(message.state === "worker-waiting"
          ? "Waiting for the cloud machine · your typing is kept"
          : "Cloud machine reconnecting · your typing is kept");
      } else if (workerState === "up" || message.state === "controller-promoted") {
        if (session.statusText) {
          session.clearStatus();
        }
        if (wasDown && workerState === "up") {
          // A dashboard that kept running behind a relay restart shows the
          // same picture; one that restarted paints anew. Either way, exact.
          requestRepaint();
        }
      }
    }
  });
}

class AttachSplash {
  private stdout: NodeJS.WriteStream;
  private label: string;
  private frame = 0;
  private status: string;
  private timer: NodeJS.Timeout | undefined;
  private active = false;
  private static FRAMES = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

  constructor(stdout: NodeJS.WriteStream, label: string) {
    this.stdout = stdout;
    this.label = label;
    this.status = `Connecting to ${label}`;
  }

  // The alternate screen is AttachSession's (held for the whole attach,
  // reconnects included); the splash only draws inside it.
  start(): void {
    if (this.active) {
      return;
    }
    this.active = true;
    this.stdout.write("\x1b[?25l");
    this.redraw();
    this.timer = setInterval(() => {
      this.frame = (this.frame + 1) % AttachSplash.FRAMES.length;
      this.redraw();
    }, 100);
    this.timer.unref?.();
  }

  setStatus(status: string): void {
    this.status = status;
    if (this.active) {
      this.redraw();
    }
  }

  redraw(): void {
    if (!this.active) {
      return;
    }
    const cols = this.stdout.columns ?? 80;
    const rows = this.stdout.rows ?? 24;
    const spinner = AttachSplash.FRAMES[this.frame] ?? "·";
    const lines = [
      `${spinner}  ${this.status}`,
      `   Ctrl+C to disconnect`,
    ];
    const top = Math.max(1, Math.floor(rows / 2) - 1);
    this.stdout.write("\x1b[2J");
    for (let i = 0; i < lines.length; i++) {
      const line = lines[i] ?? "";
      const visible = stripAnsi(line);
      const left = Math.max(1, Math.floor((cols - visible.length) / 2) + 1);
      this.stdout.write(`\x1b[${top + i};${left}H${line}`);
    }
  }

  handoff(): void {
    if (!this.active) {
      return;
    }
    this.stop();
    // Wipe the splash content first so the remote's partial frames don't
    // render on top of leftover spinner text.
    this.stdout.write("\x1b[2J\x1b[H\x1b[?25h");
  }

  stop(): void {
    if (!this.active) {
      return;
    }
    this.active = false;
    if (this.timer) {
      clearInterval(this.timer);
      this.timer = undefined;
    }
  }
}

function stripAnsi(value: string): string {
  return value.replace(/\x1b\[[0-9;]*[A-Za-z]/g, "");
}

async function maybeAutoAttach(result: JsonValue, options: CloudCommandOptions): Promise<void> {
  if (options.json || options.noAttach) {
    return;
  }
  if (!process.stdin.isTTY || !process.stdout.isTTY) {
    return;
  }
  if (process.env.RUDDER_CLOUD_NO_ATTACH === "1") {
    return;
  }
  if (!result || typeof result !== "object" || Array.isArray(result)) {
    return;
  }
  const record = result as Record<string, JsonValue>;
  if (typeof record.bootstrapCommand === "string") {
    return;
  }
  const sailId = extractSailId(record);
  if (!sailId) {
    return;
  }
  try {
    await attachWithReconnect({ kind: "sail", id: sailId, label: sailId }, options);
  } catch (error) {
    console.warn(`Could not attach to ${sailId}: ${error instanceof Error ? error.message : String(error)}`);
  }
}

function extractSailId(record: Record<string, JsonValue>): string | undefined {
  if (typeof record.id === "string" && record.id) {
    return record.id;
  }
  return undefined;
}

function printCloudHelp(): void {
  console.log(`rudder cloud

Usage:
  rudder cloud
      open this repo on your home machine (cloned from GitHub; one machine
      holds every repo) and attach to its dashboard. --snapshot uploads this
      folder to its own machine instead.
  rudder cloud home [status|start|stop|open <owner/repo>]
  rudder cloud move [runId...] [--yes]
      move this repo's unfinished agents (or just the named ones) to the
      home machine: their jj changes and conversations go along, nothing is
      pushed to GitHub, and they keep working with this laptop closed
  rudder cloud pull <runId> [--json]
      copy one of the home machine's agents to this computer, in a local
      clone of its repo: a jj workspace at its change (uncommitted edits
      included) and its conversation, listed paused in the dashboard
  rudder cloud app
      a one-time code that signs the phone app in to this account
  rudder cloud doctor
      do the cloud machine's GitHub, Claude and Codex logins work, as the same
      accounts as this laptop? names the fix for anything that differs
  rudder cloud login [--github]
      signs in with Google (or GitHub)
  rudder cloud help
  rudder cloud [name or task]
  rudder cloud launch [--home-path <path>] ["task"]
  rudder cloud byoc [ssh-host]
  rudder cloud vm ["task"]
  rudder cloud list
  rudder cloud onload [runId]
      with a runId, hands that agent off to its own cloud machine and
      resumes its conversation there; no runId uploads the current
      Rudder workspace state
  rudder cloud logs <id>
  rudder cloud attach [id|name]   (no id: reattach this repo on the home machine; a name: that project on it)
      stream the live cloud worker terminal into this pane
  rudder cloud talk <id> "<message>"
      send a message to a running instance and show its reply
  rudder cloud output <id>
      show an instance's latest output
  rudder cloud delete <id> [--yes]
      destroy an instance's cloud machine and snapshot (asks first)
  rudder cloud quickstart
      print the copy/paste setup for the whole flow
  rudder cloud slack [manifest]
      print Slack setup (one thread per instance in the shared channel)
  rudder cloud workspace [attach [id|owner/repo]|create <owner/repo>|share|status [--json]|pause <id>|resume <id>|stop <id>|delete [id] [--yes]|list]
      shared cloud workspace for this repo; \`create\` clones from GitHub (cloud-native, no local upload);
      \`delete\` destroys its machine, volume and snapshot after asking
  rudder cloud secrets [set <NAME> [value]|set --file <~/path>|list|rm <NAME>|sync]
      manage the encrypted cloud secrets vault; \`sync\` imports your local credentials once
  rudder cloud region [<fly-region>|clear]
      override worker placement (default: next to the relay for lowest typing latency)
  rudder cloud bootstrap <id>
  rudder cloud runtime [fly|byoc]
  rudder cloud setup-byoc <ssh-host>   compatibility alias
  rudder cloud setup-fly
  rudder sail [name or task]
  rudder sail list
  rudder sail pause <id>
  rudder sail resume <id>
  rudder cloud setup-github <client-id>
  rudder cloud setup-google <client-id>

Environment:
  RUDDER_CLOUD_URL              Cloud control plane URL (defaults to ${DEFAULT_CLOUD_URL})
  RUDDER_CLOUD_RUNTIME          fly, byoc, or byo-vm (overrides saved local default)
  RUDDER_CLOUD_HOME_PATHS       Extra comma-separated HOME paths to include in snapshots
  RUDDER_GITHUB_CLIENT_ID       GitHub App OAuth client ID for setup-github
  RUDDER_GITHUB_CLIENT_SECRET   GitHub App OAuth client secret for setup-github
  RUDDER_GOOGLE_CLIENT_ID       Google OAuth client ID for setup-google
  RUDDER_GOOGLE_CLIENT_SECRET   Google OAuth client secret for setup-google
`);
}

const CLOUD_ADJECTIVES = [
  "amber",
  "bright",
  "calm",
  "clear",
  "cosmic",
  "gentle",
  "golden",
  "lucky",
  "rapid",
  "silver",
  "steady",
  "swift",
];

const CLOUD_NOUNS = [
  "atlas",
  "harbor",
  "signal",
  "summit",
  "orbit",
  "ranger",
  "river",
  "rocket",
  "sparrow",
  "station",
  "voyager",
  "wave",
];

function randomCloudName(): string {
  const seed = Date.now() + process.pid + Math.floor(Math.random() * 1_000_000);
  return [
    CLOUD_ADJECTIVES[Math.abs(seed) % CLOUD_ADJECTIVES.length],
    CLOUD_NOUNS[Math.abs(Math.floor(seed / CLOUD_ADJECTIVES.length)) % CLOUD_NOUNS.length],
  ].join("-");
}

function cloudNameFromTask(task: string): string {
  const slug = task
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 36)
    .replace(/-+$/g, "");
  return slug || randomCloudName();
}

// ---------------------------------------------------------------------------
// Home machine: `rudder cloud` in a repo opens it on the account's one cloud
// machine (cloning from GitHub on first use) and attaches to its dashboard.
// GitHub is the source of truth: work comes back as pushed branches and draft
// PRs, so nothing is uploaded from this laptop except .env files you approve.
// ---------------------------------------------------------------------------

type HomeOpenResponse = {
  home?: Record<string, JsonValue>;
  workspace?: Record<string, JsonValue>;
};

async function repoOpenOnHome(client: CloudClient, slug: string): Promise<boolean> {
  try {
    const result = await client.request<{ projects?: Record<string, JsonValue>[] }>("/api/rudder/home", { method: "GET" });
    return (result.projects ?? []).some((project) => String(project.repo ?? "").toLowerCase() === slug.toLowerCase());
  } catch {
    return false;
  }
}

async function openOnHome(forceSnapshot: boolean, options: CloudCommandOptions): Promise<void> {
  if (forceSnapshot) {
    await workspaceAttach([], options);
    return;
  }
  const repoRoot = findRepoRoot();
  const slug = await githubSlugFromOrigin(repoRoot);
  if (!slug) {
    if (!options.json) {
      process.stderr.write("This repo has no GitHub origin, so it can't be cloned on the home machine; uploading a snapshot workspace instead.\n");
    }
    await workspaceAttach([], options);
    return;
  }
  const client = await cloudClient({ requireToken: true });
  const name = slug.split("/")[1];
  // Coming back (laptop reopened, terminal closed): a repo already open on
  // the home machine reattaches straight away, like `rudder cloud attach`.
  // Credentials are still compared, and running local agents still offered a
  // move, because those are about this laptop now.
  const reattach = options.reattach || await repoOpenOnHome(client, slug);
  if (!options.json) {
    if (!reattach) {
      await warnLocalOnlyWork(repoRoot);
    }
    await ensureVaultGithubToken(client, slug);
    await ensureVaultAws(client);
    if (!reattach) {
      await offerEnvFilesToVault(client, repoRoot, name);
    }
    await offerToMoveAgents(client, repoRoot, slug);
    process.stderr.write(`Opening ${slug} on your home machine...\n`);
  }
  const branch = await pushedBranch(repoRoot);
  const result = await client.request<HomeOpenResponse>("/api/rudder/home/open", {
    method: "POST",
    body: { repo: slug, ...(branch ? { branch } : {}) },
  });
  const workspaceId = typeof result.workspace?.id === "string" ? result.workspace.id : "";
  if (!workspaceId) {
    throw new Error("the control plane did not return a workspace for this repo");
  }
  if (options.json) {
    printJson(result as unknown as JsonValue);
    return;
  }
  if (!process.stdin.isTTY || !process.stdout.isTTY) {
    process.stderr.write(`${slug} is open on the home machine (workspace ${workspaceId}). Run \`rudder cloud\` from a terminal to attach.\n`);
    return;
  }
  if (process.env.RUDDER_CLOUD_NO_ATTACH === "1") {
    return;
  }
  process.stderr.write(`Phone: ${appUrl(client.baseUrl)}\n`);
  await reportCredentialParity(client, { problemsOnly: true }).catch(() => undefined);
  await attachWithReconnect({ kind: "workspace", id: workspaceId, label: `${name} on home` }, options, async () => {
    // Wake the machine again if it was what went away.
    await client.request<HomeOpenResponse>("/api/rudder/home/open", { method: "POST", body: { repo: slug, ...(branch ? { branch } : {}) } });
  });
}

// The cloud clones from GitHub, so anything not pushed is not there. Say so
// plainly; it is a warning, not a gate.
async function warnLocalOnlyWork(repoRoot: string): Promise<void> {
  const status = await runCommand("git", ["status", "--porcelain"], { cwd: repoRoot, allowFailure: true });
  const changed = status.code === 0 ? status.stdout.split("\n").filter((line) => line.trim()).length : 0;
  let unpushed = 0;
  let branchMissing = "";
  const upstream = await runCommand("git", ["rev-list", "--count", "@{u}..HEAD"], { cwd: repoRoot, allowFailure: true });
  if (upstream.code === 0) {
    unpushed = Number(upstream.stdout.trim()) || 0;
  } else {
    const branch = await runCommand("git", ["rev-parse", "--abbrev-ref", "HEAD"], { cwd: repoRoot, allowFailure: true });
    const name = branch.stdout.trim();
    if (name && name !== "HEAD") {
      branchMissing = name;
    }
  }
  const parts: string[] = [];
  if (changed > 0) {
    parts.push(`${changed} uncommitted file${changed === 1 ? "" : "s"}`);
  }
  if (unpushed > 0) {
    parts.push(`${unpushed} unpushed commit${unpushed === 1 ? "" : "s"}`);
  }
  if (branchMissing) {
    parts.push(`branch ${branchMissing} is not on GitHub`);
  }
  if (parts.length > 0) {
    process.stderr.write(`Heads up: ${parts.join(", ")}. The cloud clones from GitHub, so it won't see that. Push first, or use \`rudder cloud --snapshot\` to upload this folder instead.\n`);
  }
}

// The cloud's AWS is whatever ~/.aws the vault holds. When this laptop's copy
// has changed since (new keys, a new profile), offer to bring the vault level.
// The vault's copy of the laptop's agent and AWS config must match the
// laptop: the cloud machine rewrites these files from the vault every boot,
// and a drifted copy means different behavior there (verbose output, another
// default model, hooks that no longer exist). Compared by checksum; AWS is
// asked about once, everything else that is already shared just follows.
const LAPTOP_CONFIG_FILES = [
  "~/.claude/settings.json",
  "~/.claude/CLAUDE.md",
  "~/.codex/config.toml",
  "~/.codex/AGENTS.md",
  "~/.codex/hooks.json",
  // Service logins agents use from scripts (Kalshi signs requests with this key).
  "~/.kalshi/config.json",
  "~/.kalshi/private_key.pem",
];

async function laptopConfigFiles(): Promise<{ name: string; content: Buffer }[]> {
  const names = [...LAPTOP_CONFIG_FILES, "~/.aws/config", "~/.aws/credentials"];
  // Hook scripts the settings above point at.
  for (const dir of [".claude/hooks", ".codex/libra-hooks"]) {
    for (const entry of await fsp.readdir(path.join(os.homedir(), dir), { withFileTypes: true }).catch(() => [])) {
      if (entry.isFile()) {
        names.push(`~/${dir}/${entry.name}`);
      }
    }
  }
  const out: { name: string; content: Buffer }[] = [];
  for (const name of names) {
    const content = await fsp.readFile(path.join(os.homedir(), name.slice(2))).catch(() => null);
    if (content && content.length > 0 && content.length <= MAX_SECRET_VALUE_BYTES) {
      out.push({ name, content });
    }
  }
  return out;
}

async function ensureVaultAws(client: CloudClient): Promise<void> {
  const present = await laptopConfigFiles();
  if (present.length === 0 || !isTty()) {
    return;
  }
  const vault = await client.request<{ secrets?: { name: string; sha256?: string }[] }>("/api/rudder/secrets", { method: "GET" }).catch(() => null);
  if (!vault) {
    return;
  }
  const stored = new Map((vault.secrets ?? []).map((secret) => [secret.name, secret.sha256 ?? ""]));
  const sha = (content: Buffer) => createHash("sha256").update(content).digest("hex").slice(0, 8);
  const changed = present.filter((file) => stored.get(file.name) !== sha(file.content));
  if (changed.length === 0) {
    return;
  }
  const isAws = (name: string) => name.startsWith("~/.aws/");
  const awsShared = [...stored.keys()].some(isAws);
  let toSend = changed.filter((file) => !isAws(file.name) || awsShared);
  const newAws = changed.filter((file) => isAws(file.name) && !awsShared);
  if (newAws.length > 0) {
    const yes = await promptConfirm(`Copy your AWS setup (${newAws.map((file) => file.name).join(", ")}) to the cloud vault so cloud agents use the same AWS?`, true);
    if (yes) {
      toSend = [...toSend, ...newAws];
    }
  }
  for (const file of toSend) {
    await client.request<JsonValue>("/api/rudder/secrets/item", {
      method: "PUT",
      body: { name: file.name, kind: "file", filePath: file.name, valueBase64: file.content.toString("base64"), source: "rudder cloud" },
    });
  }
  if (toSend.length > 0) {
    process.stderr.write(`Updated the cloud copy of ${toSend.map((file) => file.name).join(", ")} to match this laptop.\n`);
  }
}

async function localGithubToken(): Promise<string | null> {
  if (!commandExists("gh")) {
    return null;
  }
  const result = await runCommand("gh", ["auth", "token"], { allowFailure: true });
  const token = result.stdout.trim();
  return result.code === 0 && /^(gh[opsu]_|github_pat_)[A-Za-z0-9_]+$/.test(token) ? token : null;
}

// The cloud machine clones from GitHub, so a private repo needs a GitHub token
// in the vault. gh keeps its token in the OS keychain (hosts.yml has none), so
// it never arrived with the synced gh config; offer to put it there.
async function ensureVaultGithubToken(client: CloudClient, slug: string, interactive = true): Promise<void> {
  const vault = await client.request<{ secrets?: { name: string }[] }>("/api/rudder/secrets", { method: "GET" }).catch(() => null);
  if (!vault || (vault.secrets ?? []).some((secret) => secret.name === "GH_TOKEN" || secret.name === "GITHUB_TOKEN")) {
    return;
  }
  const token = await localGithubToken();
  if (!token) {
    process.stderr.write(`The cloud vault has no GitHub token, so private repos can't be cloned there. Run \`gh auth login\` here, or \`rudder cloud secrets set GH_TOKEN\`.\n`);
    return;
  }
  if (!isTty() || !interactive) {
    process.stderr.write("The cloud vault has no GitHub token; private repos will fail to clone. Run `rudder cloud secrets sync` from a terminal.\n");
    return;
  }
  const yes = await promptConfirm(`The cloud machine needs GitHub access to clone ${slug} (and to push branches and open PRs). Copy your GitHub CLI token into the cloud vault?`, true);
  if (!yes) {
    return;
  }
  await client.request<JsonValue>("/api/rudder/secrets/item", {
    method: "PUT",
    body: { name: "GH_TOKEN", kind: "env", valueBase64: Buffer.from(token, "utf8").toString("base64"), source: "rudder cloud" },
  });
  process.stderr.write("Stored GH_TOKEN in the cloud vault.\n");
}

// ---------------------------------------------------------------------------
// Moving agents to the home machine (src/move-home.ts has the mechanics).
// `rudder cloud move [runId...]` is what the dashboard runs for H, the quit
// offer and /handoff; `rudder cloud` offers it before attaching. A row moves
// only when the home machine reports it staged: then, and only then, its
// local run.json says "migrated", which is what the dashboard watches for.
// ---------------------------------------------------------------------------

type HomeMoveJob = {
  move?: {
    id?: string;
    status?: string;
    detail?: string | null;
    error?: string | null;
    result?: { adopted?: Array<{ runId: string; resumable?: boolean; alreadyAdopted?: boolean }>; failed?: Array<{ runId: string; error: string }> } | null;
  };
};

const HOME_MOVE_POLL_MS = 1500;
const HOME_MOVE_DEADLINE_MS = 15 * 60 * 1000;

// `rudder cloud pull <runId>`: the reverse of move, for one agent (src/cloud-pull.ts).
async function pullCommand(args: string[], options: CloudCommandOptions): Promise<void> {
  const client = await cloudClient({ requireToken: true });
  const json = options.json || args.includes("--json");
  const pulled = await pullFromHome(args, { json }, { client, githubSlug: githubSlugFromOrigin });
  if (json) {
    printJson({ path: pulled.path, runId: pulled.runId, title: pulled.title });
    return;
  }
  console.log(`Pulled "${pulled.title}" to ${pulled.path}`);
  console.log(pulled.resumable
    ? "It is paused in the dashboard here; Enter on its row continues the conversation."
    : "It is paused in the dashboard here; its conversation did not come along, so Enter starts it fresh on its work.");
}

async function moveCommand(args: string[], options: CloudCommandOptions): Promise<void> {
  const yes = options.yes || args.includes("--yes") || args.includes("-y");
  const runIds = args.filter((arg) => !arg.startsWith("-"));
  const repoRoot = findRepoRoot();
  const slug = await githubSlugFromOrigin(repoRoot);
  if (!slug) {
    throw new Error("This repo has no GitHub origin, so the home machine can't clone it. `rudder cloud workspace attach` moves agents into a snapshot workspace instead.");
  }
  const candidates = await findMoveCandidates(repoRoot, runIds.length > 0 ? runIds : undefined);
  const missing = runIds.filter((id) => !candidates.some((candidate) => candidate.runId === id));
  if (missing.length > 0) {
    throw new Error(`Not movable (finished, not in a workspace of its own, or unknown): ${missing.join(", ")}`);
  }
  if (candidates.length === 0) {
    if (!options.json) {
      process.stderr.write("No unfinished agents with workspaces of their own to move.\n");
    }
    return;
  }
  if (!yes && !options.json) {
    if (!isTty()) {
      throw new Error("Refusing to move agents without a terminal to confirm in; pass --yes.");
    }
    process.stderr.write(`${describeMoveCandidates(candidates)}\n`);
    if (!(await promptConfirm(`Move ${agentCount(candidates.length)} to the home machine?`, true))) {
      process.stderr.write("Nothing moved.\n");
      return;
    }
  }
  const client = await cloudClient({ requireToken: true });
  if (!options.json) {
    await ensureVaultGithubToken(client, slug, !yes);
  }
  const moved = await moveAgentsToHome(client, repoRoot, slug, candidates, options);
  if (options.json) {
    printJson(moved as unknown as JsonValue);
  }
  if (moved.adopted.length === 0) {
    process.exitCode = 1;
  }
}

function agentCount(count: number): string {
  return `${count} agent${count === 1 ? "" : "s"}`;
}

function describeMoveCandidates(candidates: MoveCandidate[]): string {
  return candidates
    .map((candidate) => `  ${candidate.status.padEnd(8)} ${(candidate.taskSummary || candidate.task || candidate.runId).replace(/\s+/g, " ").slice(0, 72)}`)
    .join("\n");
}

/** Bundle, upload, wait for the home machine to stage them, then hand the local records over. */
export async function moveAgentsToHome(
  client: CloudClient,
  repoRoot: string,
  slug: string,
  candidates: MoveCandidate[],
  options: CloudCommandOptions,
): Promise<{ adopted: string[]; failed: Array<{ runId: string; error: string }> }> {
  const say = (line: string) => {
    if (!options.json) {
      process.stderr.write(`${line}\n`);
    }
  };
  const stageDir = await fsp.mkdtemp(path.join(os.tmpdir(), "rudder-move-stage-"));
  const archiveDir = await fsp.mkdtemp(path.join(os.tmpdir(), "rudder-move-"));
  let job: HomeMoveJob;
  try {
    say(`Packing ${agentCount(candidates.length)} (their jj changes and conversations)...`);
    await buildMoveBundle(repoRoot, candidates, stageDir);
    const archivePath = path.join(archiveDir, "move.tgz");
    await runCommand("tar", ["-czf", archivePath, "-C", stageDir, "."], { cwd: stageDir });
    const branch = await pushedBranch(repoRoot);
    job = await client.request<HomeMoveJob>("/api/rudder/home/move", {
      method: "POST",
      body: {
        repo: slug,
        ...(branch ? { branch } : {}),
        bundle: await fsp.readFile(archivePath, "base64"),
      },
    });
  } finally {
    await fsp.rm(stageDir, { recursive: true, force: true });
    await fsp.rm(archiveDir, { recursive: true, force: true });
  }
  const moveId = job.move?.id;
  if (!moveId) {
    throw new Error("the control plane did not start the move");
  }
  const deadline = Date.now() + HOME_MOVE_DEADLINE_MS;
  let lastDetail = "";
  while (job.move?.status !== "done" && job.move?.status !== "failed") {
    if (Date.now() > deadline) {
      throw new Error("the home machine did not take the agents in time; they stay here");
    }
    const detail = job.move?.detail ?? "";
    if (detail && detail !== lastDetail) {
      say(detail);
      lastDetail = detail;
    }
    await new Promise((resolve) => setTimeout(resolve, HOME_MOVE_POLL_MS));
    job = await client.request<HomeMoveJob>(`/api/rudder/home/move/${encodeURIComponent(moveId)}`, { method: "GET" });
  }
  if (job.move?.status === "failed") {
    throw new Error(`the home machine could not take the agents: ${job.move.error || "unknown error"}`);
  }
  const adopted = (job.move?.result?.adopted ?? []).map((entry) => entry.runId);
  const failed = job.move?.result?.failed ?? [];
  await markRunsMovedHome(repoRoot, adopted, slug);
  if (adopted.length > 0) {
    const fresh = (job.move?.result?.adopted ?? []).filter((entry) => entry.resumable === false).length;
    say(`Moved ${agentCount(adopted.length)} to the home machine. They keep working there with this laptop closed.`);
    if (fresh > 0) {
      say(`${agentCount(fresh)} had no saved conversation and restart from a handoff over their diff.`);
    }
    say(`Watch them: \`rudder cloud\` here, or on your phone: ${appUrl(client.baseUrl)}`);
  }
  for (const entry of failed) {
    say(`Could not move ${entry.runId}: ${entry.error}. It stays here.`);
  }
  return { adopted, failed };
}

/**
 * `rudder cloud` in a repo with agents: offer to take them along. A live
 * dashboard owns its agents' processes, so it does the move (it pauses them
 * first); with no dashboard running the agents are already stopped and this
 * process can move them itself.
 */
async function offerToMoveAgents(client: CloudClient, repoRoot: string, slug: string): Promise<void> {
  if (!isTty() || process.env.RUDDER_CLOUD_RUNTIME === "home" || process.env.RUDDER_CLOUD_NO_MOVE_OFFER === "1") {
    return;
  }
  const candidates = await findMoveCandidates(repoRoot).catch(() => []);
  if (candidates.length === 0) {
    return;
  }
  const dashboardLive = await dashboardServing(repoRoot);
  const running = candidates.filter((candidate) => candidate.status === "running");
  if (dashboardLive && running.length > 0) {
    const yes = await promptConfirm(`${agentCount(running.length)} ${running.length === 1 ? "is" : "are"} running here. Move them to the home machine?`, true);
    if (!yes) {
      return;
    }
    const receipt = await requestDashboardMove(repoRoot);
    process.stderr.write(receipt === "failed"
      ? "The dashboard could not start the move; press H there to try again.\n"
      : "The dashboard is moving them; they appear on the home machine as they land.\n");
    return;
  }
  if (dashboardLive) {
    return;
  }
  process.stderr.write(`${describeMoveCandidates(candidates)}\n`);
  const yes = await promptConfirm(`${agentCount(candidates.length)} ${candidates.length === 1 ? "was" : "were"} left unfinished here. Move them to the home machine?`, true);
  if (!yes) {
    return;
  }
  try {
    await moveAgentsToHome(client, repoRoot, slug, candidates, {});
  } catch (error) {
    process.stderr.write(`${error instanceof Error ? error.message : String(error)}\n`);
  }
}

async function dashboardServing(repoRoot: string): Promise<boolean> {
  let socketPath: string;
  try {
    socketPath = await fsp.realpath(path.join(repoRoot, ".rudder", "attach.sock"));
  } catch {
    return false;
  }
  const net = await import("node:net");
  return await new Promise<boolean>((resolve) => {
    const socket = net.connect(socketPath);
    const timer = setTimeout(() => {
      socket.destroy();
      resolve(false);
    }, 500);
    socket.once("connect", () => {
      clearTimeout(timer);
      socket.destroy();
      resolve(true);
    });
    socket.once("error", () => {
      clearTimeout(timer);
      resolve(false);
    });
  });
}

/** Ask the live dashboard (through its steer inbox) to move every agent it is running. */
async function requestDashboardMove(repoRoot: string): Promise<"delivered" | "failed" | "queued"> {
  const requestId = randomUUID();
  const inbox = path.join(repoRoot, ".rudder", "steer");
  await ensureDir(inbox);
  const file = path.join(inbox, `${String(Date.now()).padStart(13, "0")}-move-home-${requestId}.json`);
  const temp = `${file}.${process.pid}.tmp`;
  await fsp.writeFile(temp, JSON.stringify({ requestId, kind: "move-home", taskId: "", instruction: "move-home", ts: nowIso() }));
  await fsp.rename(temp, file);
  const receiptPath = path.join(repoRoot, ".rudder", "steer-receipts", `${requestId}.json`);
  const deadline = Date.now() + 10_000;
  while (Date.now() < deadline) {
    await new Promise((resolve) => setTimeout(resolve, 250));
    const receipt = await readJson<{ status?: string }>(receiptPath);
    if (receipt?.status === "delivered" || receipt?.status === "failed") {
      return receipt.status;
    }
  }
  return "queued";
}

// ---------------------------------------------------------------------------
// Credential parity: the cloud machine should be able to act as you, as the
// same accounts you use here. The home supervisor checks itself (one real
// request per tool, not just "a login file exists"); this compares that with
// this laptop and names the fix for anything that differs.
// ---------------------------------------------------------------------------

type CredentialCheck = { ok?: boolean; account?: string | null; detail?: string | null };
type ParityRow = { tool: string; local: string | null; cloud: CredentialCheck | null; status: "match" | "missing" | "different" | "unknown"; fix?: string };

async function localIdentities(): Promise<Record<string, string | null>> {
  const [github, claude] = await Promise.all([
    commandExists("gh")
      ? runCommand("gh", ["api", "user", "--jq", ".login"], { allowFailure: true }).then((r) => (r.code === 0 ? r.stdout.trim() || null : null))
      : Promise.resolve(null),
    commandExists("claude")
      ? runCommand("claude", ["auth", "status", "--json"], { allowFailure: true }).then((r) => {
          try {
            const parsed = JSON.parse(r.stdout) as { loggedIn?: boolean; email?: string };
            return parsed.loggedIn ? parsed.email ?? "logged in" : null;
          } catch {
            return null;
          }
        })
      : Promise.resolve(null),
  ]);
  const aws = commandExists("aws")
    ? await runCommand("aws", ["sts", "get-caller-identity", "--query", "Arn", "--output", "text"], { allowFailure: true })
        .then((r) => (r.code === 0 && r.stdout.trim().startsWith("arn:") ? r.stdout.trim() : null))
    : null;
  let codex: string | null = null;
  try {
    const auth = JSON.parse(await fsp.readFile(path.join(os.homedir(), ".codex", "auth.json"), "utf8")) as { tokens?: { id_token?: string } };
    const payload = (auth.tokens?.id_token ?? "").split(".")[1];
    codex = payload ? (JSON.parse(Buffer.from(payload, "base64url").toString("utf8")) as { email?: string }).email ?? null : null;
  } catch {
    codex = null;
  }
  return { github, claude, codex, aws };
}

const CREDENTIAL_FIXES: Record<string, string> = {
  github: "run `rudder cloud` in a repo here (it offers to copy `gh auth token` into the vault)",
  claude: "run `rudder cloud`, then `/login` in a Claude pane there (the cloud keeps its own login)",
  codex: "run `rudder cloud`, then `$ codex login --device-auth` in the task pane there",
  aws: "run `rudder cloud` in a repo here: it offers to copy your current ~/.aws into the vault",
};

async function credentialParity(client: CloudClient): Promise<{ rows: ParityRow[]; checkedAt: string | null }> {
  const [local, home] = await Promise.all([
    localIdentities(),
    client.request<{ home: { credentials?: Record<string, CredentialCheck> & { checkedAt?: string } } | null }>("/api/rudder/home", { method: "GET" }),
  ]);
  const cloud = home.home?.credentials ?? null;
  const rows: ParityRow[] = (["github", "claude", "codex", "aws"] as const).map((tool) => {
    const remote = (cloud?.[tool] as CredentialCheck | undefined) ?? null;
    const mine = local[tool];
    if (!remote) {
      return { tool, local: mine, cloud: null, status: "unknown" };
    }
    if (!remote.ok) {
      // ENOENT: the CLI was being updated when the machine checked; it rechecks.
      const fix = /ENOENT/.test(remote.detail ?? "")
        ? `${tool} was updating when the cloud checked; run \`rudder cloud doctor\` again in a minute`
        : CREDENTIAL_FIXES[tool];
      return { tool, local: mine, cloud: remote, status: mine ? "missing" : "unknown", fix };
    }
    if (mine && remote.account && remote.account.toLowerCase() !== mine.toLowerCase()) {
      return { tool, local: mine, cloud: remote, status: "different", fix: CREDENTIAL_FIXES[tool] };
    }
    return { tool, local: mine, cloud: remote, status: "match" };
  });
  return { rows, checkedAt: typeof cloud?.checkedAt === "string" ? cloud.checkedAt : null };
}

async function reportCredentialParity(client: CloudClient, { problemsOnly }: { problemsOnly: boolean }): Promise<number> {
  const { rows, checkedAt } = await credentialParity(client);
  const problems = rows.filter((row) => row.status === "missing" || row.status === "different");
  if (problemsOnly && problems.length === 0) {
    return 0;
  }
  const label = (tool: string) => ({ github: "GitHub", claude: "Claude", codex: "Codex", aws: "AWS" })[tool] ?? tool;
  for (const row of problemsOnly ? problems : rows) {
    const here = row.local ?? "not logged in";
    const there = row.cloud ? (row.cloud.ok ? row.cloud.account ?? "logged in" : `not working (${row.cloud.detail ?? "unknown"})`) : "not checked yet";
    const mark = row.status === "match" ? "✓" : row.status === "unknown" ? "·" : row.status === "different" ? "⚠" : "✗";
    process.stderr.write(`${mark} ${label(row.tool).padEnd(7)} here: ${here}   cloud: ${there}\n`);
    if (row.fix) {
      process.stderr.write(`    fix: ${row.fix}\n`);
    }
  }
  if (!problemsOnly && !checkedAt) {
    process.stderr.write("The home machine has not finished checking its logins yet; try again in a minute.\n");
  }
  return problems.length;
}

// `rudder cloud sync` (and `/sync` in the dashboard): GitHub is the meeting
// point, so this checkout and the home machine's clone each fetch and move to
// the newest default branch when that is safe, and the vault's copy of this
// laptop's config is brought level. Unpushed or uncommitted work on either side
// is reported, never pushed or overwritten. `--all` syncs every repo open on
// the home machine.
async function cloudSync(args: string[], options: CloudCommandOptions): Promise<void> {
  if (process.env.RUDDER_CLOUD_RUNTIME === "home") {
    await homeMachineSync(args, options);
    return;
  }
  const client = await cloudClient({ requireToken: true });
  const all = args.includes("--all");
  const named = args.find((arg) => !arg.startsWith("-"));
  const root = named ? await localCheckoutFor(client, named, options) : findRepoRoot();
  const slug = await githubSlugFromOrigin(root);
  const lines: string[] = [];
  let local: CheckoutSyncReport | null = null;
  let work: { bundle: string; head: string } | undefined;
  const scratch = await fsp.mkdtemp(path.join(os.tmpdir(), "rudder-sync-"));
  if (slug) {
    local = await syncCheckout(root);
    lines.push("");
    // This laptop's work GitHub lacks goes to the cloud machine directly.
    const sent = await exportWork(root, path.join(scratch, "out.bundle")).catch((error: unknown) => {
      lines.push(`work: could not bundle this laptop's work (${error instanceof Error ? error.message : String(error)})`);
      return null;
    });
    if (sent) {
      local.work = { sent };
      work = { bundle: (await fsp.readFile(path.join(scratch, "out.bundle"))).toString("base64"), head: sent.head };
    }
  } else if (!all) {
    throw new Error("This repo has no GitHub origin, so there is nothing on the home machine to sync it with. `rudder cloud sync --all` syncs every repo there.");
  }
  await ensureVaultAws(client).catch((error: unknown) => {
    lines.push(`config: could not update the vault (${error instanceof Error ? error.message : String(error)})`);
  });
  if (!options.json) {
    process.stderr.write("Asking the cloud machine (waking it if it is asleep)...\n");
  }
  const cloud = await client.request<{ results?: { repo: string; report?: CheckoutSyncReport; error?: string }[] }>("/api/rudder/home/sync", {
    method: "POST",
    body: all || !slug ? {} : { repos: [slug], ...(work ? { work } : {}) },
  }).catch((error: unknown) => {
    const message = error instanceof Error ? error.message : String(error);
    throw new Error(message === "fetch failed" ? "the cloud machine did not answer in time (it may be restarting); run /sync again in a minute" : message);
  }).finally(() => fsp.rm(scratch, { recursive: true, force: true }).catch(() => undefined));
  const results = cloud.results ?? [];
  // And the cloud's work comes back here.
  const mine = slug ? results.find((result) => result.repo.toLowerCase() === slug.toLowerCase()) : undefined;
  const incoming = mine?.report?.work?.sent as { head: string; download?: string } | undefined;
  if (local && incoming?.download) {
    const dir = await fsp.mkdtemp(path.join(os.tmpdir(), "rudder-sync-in-"));
    try {
      const file = path.join(dir, "in.bundle");
      await fsp.writeFile(file, await client.download(incoming.download));
      local.work = { ...local.work, received: await importWork(root, file, incoming.head, "cloud") };
    } catch (error) {
      local.work = { ...local.work, received: { from: "cloud", head: incoming.head, outcome: "failed", note: `could not take the cloud's work (${error instanceof Error ? error.message : String(error)})` } };
    } finally {
      await fsp.rm(dir, { recursive: true, force: true }).catch(() => undefined);
    }
  }
  // Work the other side already had was not news: report only what moved.
  if (local?.work?.sent && mine?.report?.work?.received?.outcome === "have") {
    delete local.work.sent;
  }
  if (mine?.report?.work?.sent && local?.work?.received?.outcome === "have") {
    delete mine.report.work.sent;
  }
  for (const side of [local?.work, mine?.report?.work]) {
    if (side?.received?.outcome === "have") {
      delete side.received;
    }
  }
  if (local) {
    lines[0] = describeSync(`this laptop (${slug})`, local);
  }
  if (options.json) {
    printJson({ local, cloud: results } as unknown as JsonValue);
    return;
  }
  for (const result of results) {
    lines.push(result.report ? describeSync(`cloud (${result.repo})`, result.report) : `cloud (${result.repo}): ${result.error ?? "no answer"}`);
  }
  if (slug && results.length === 0) {
    lines.push(`cloud: ${slug} is not open on the cloud machine (\`rudder cloud\` opens it there)`);
  }
  // The agents, on each side. Cloud ones are listed from what the home machine
  // last reported; laptop ones that are still running are what /sync move (or H
  // in the dashboard) would hand over.
  const homeView = await client.request<{ projects?: { repo?: string; agents?: { title?: string; status?: string; asks?: unknown[] }[] }[] }>("/api/rudder/home", { method: "GET" }).catch(() => null);
  for (const project of homeView?.projects ?? []) {
    if (!project.repo || !results.some((result) => result.repo.toLowerCase() === project.repo!.toLowerCase())) {
      continue;
    }
    lines.push(...describeCloudAgents(project.repo, project.agents ?? []));
  }
  if (slug) {
    const running = await findMoveCandidates(root).catch(() => []);
    if (running.length > 0) {
      lines.push(`${running.length} agent(s) still running on this laptop: ${running.slice(0, 4).map((run) => `"${(run.taskSummary || run.task).slice(0, 50)}"`).join(", ")}${running.length > 4 ? ", ..." : ""}. They stop when the laptop sleeps; \`/sync move\` (or H) moves them to the cloud machine.`);
    }
  }
  console.log(lines.filter(Boolean).join("\n"));
}

// `/sync` typed on the cloud machine itself. The laptop path needs a CLI login
// the home never has and would sync the machine with itself; here the useful
// half is this machine's clone(s) taking the newest default branch from GitHub
// when nothing would be lost. Work travels between devices from the laptop side.
async function homeMachineSync(args: string[], options: CloudCommandOptions): Promise<void> {
  const root = findRepoRoot();
  const roots = [root];
  if (args.includes("--all")) {
    const parent = path.dirname(root);
    for (const entry of await fsp.readdir(parent, { withFileTypes: true }).catch(() => [])) {
      const dir = path.join(parent, entry.name);
      if (entry.isDirectory() && dir !== root && (await pathExists(path.join(dir, ".git")))) {
        roots.push(dir);
      }
    }
  }
  const results: { repo: string; report?: CheckoutSyncReport; error?: string }[] = [];
  for (const dir of roots) {
    const repo = (await githubSlugFromOrigin(dir)) ?? path.basename(dir);
    try {
      results.push({ repo, report: await syncCheckout(dir) });
    } catch (error) {
      results.push({ repo, error: error instanceof Error ? error.message : String(error) });
    }
  }
  if (options.json) {
    printJson({ local: null, cloud: results } as unknown as JsonValue);
    return;
  }
  const lines = results.map((result) => (result.report ? describeSync(`cloud machine (${result.repo})`, result.report) : `cloud machine (${result.repo}): ${result.error}`));
  lines.push("To bring a laptop's work here (or this machine's work to a laptop), run /sync on the laptop.");
  console.log(lines.join("\n"));
}

// `rudder cloud sync <name>`: the laptop's clone of a repo open on the cloud
// machine, cloned from GitHub into ~/code/<name> the first time.
async function localCheckoutFor(client: CloudClient, name: string, options: CloudCommandOptions): Promise<string> {
  const view = await client.request<{ projects?: { repo?: string }[] }>("/api/rudder/home", { method: "GET" });
  const wanted = name.replace(/\.git$/, "").toLowerCase();
  const slug = (view.projects ?? []).map((project) => project.repo ?? "").find((repo) => repo.toLowerCase() === wanted || repo.split("/").pop()?.toLowerCase() === wanted);
  if (!slug) {
    throw new Error(`${name} is not open on the cloud machine.`);
  }
  const dir = path.join(os.homedir(), "code", slug.split("/").pop()!);
  if (await pathExists(path.join(dir, ".git"))) {
    const existing = await githubSlugFromOrigin(dir);
    if (existing?.toLowerCase() !== slug.toLowerCase()) {
      throw new Error(`${shortenHome(dir)} already exists and is not a clone of ${slug}.`);
    }
    return dir;
  }
  if (await pathExists(dir)) {
    throw new Error(`${shortenHome(dir)} already exists and is not a git checkout.`);
  }
  if (!options.json) {
    process.stderr.write(`Cloning ${slug} into ${shortenHome(dir)}...\n`);
  }
  await fsp.mkdir(path.dirname(dir), { recursive: true });
  const cloned = commandExists("gh")
    ? await runCommand("gh", ["repo", "clone", slug, dir], { allowFailure: true })
    : await runCommand("git", ["clone", `https://github.com/${slug}.git`, dir], { allowFailure: true });
  if (cloned.code !== 0) {
    throw new Error(`could not clone ${slug}: ${(cloned.stderr || cloned.stdout).trim().split("\n").pop()}`);
  }
  return dir;
}

function describeCloudAgents(repo: string, agents: { title?: string; status?: string; asks?: unknown[] }[]): string[] {
  const live = agents.filter((agent) => !["merged", "stopped", "deleted"].includes(String(agent.status)));
  if (live.length === 0) {
    return [`cloud agents (${repo}): none running`];
  }
  const label = (agent: { status?: string; asks?: unknown[] }) => (Array.isArray(agent.asks) && agent.asks.length > 0 ? "needs you" : String(agent.status || "?"));
  const order = ["needs you", "waiting", "working", "starting", "review", "paused", "done", "failed"];
  const sorted = [...live].sort((a, b) => (order.indexOf(label(a)) + 1 || 99) - (order.indexOf(label(b)) + 1 || 99));
  return [
    `cloud agents (${repo}): ${live.length}`,
    ...sorted.slice(0, 8).map((agent) => `  ${label(agent).padEnd(10)} ${(agent.title || "").slice(0, 70)}`),
    ...(sorted.length > 8 ? [`  ... and ${sorted.length - 8} more (\`rudder cloud\` to open them)`] : []),
  ];
}

async function cloudDoctor(options: CloudCommandOptions): Promise<void> {
  const client = await cloudClient({ requireToken: true });
  if (options.json) {
    printJson((await credentialParity(client)) as unknown as JsonValue);
    return;
  }
  const problems = await reportCredentialParity(client, { problemsOnly: false });
  if (problems > 0) {
    process.exitCode = 1;
  }
}

// The current branch, if GitHub has it; otherwise the default branch is used.
async function pushedBranch(repoRoot: string): Promise<string | undefined> {
  const branch = (await runCommand("git", ["rev-parse", "--abbrev-ref", "HEAD"], { cwd: repoRoot, allowFailure: true })).stdout.trim();
  if (!branch || branch === "HEAD") {
    return undefined;
  }
  const remote = await runCommand("git", ["ls-remote", "--heads", "origin", branch], { cwd: repoRoot, allowFailure: true });
  return remote.code === 0 && remote.stdout.trim() ? branch : undefined;
}

// A GitHub clone has no .env files (they are gitignored). Offer to put this
// repo's into the vault, stored at ~/code/<repo>/<path> so the home machine
// writes them into its clone.
async function offerEnvFilesToVault(client: CloudClient, repoRoot: string, repoName: string): Promise<void> {
  if (!isTty()) {
    return;
  }
  const listed = await runCommand("git", ["ls-files", "--others", "--ignored", "--exclude-standard", "--directory"], {
    cwd: repoRoot,
    allowFailure: true,
  });
  if (listed.code !== 0) {
    return;
  }
  const envFiles = listed.stdout
    .split("\n")
    .map((line) => line.trim())
    .filter((line) => line && !line.endsWith("/"))
    .filter((line) => /^\.env(\.[\w.-]+)?$/.test(path.basename(line)))
    .filter((line) => !/\.(example|sample|template|dist)$/.test(line))
    .filter((line) => !line.split("/").includes("node_modules"))
    .slice(0, 20);
  if (envFiles.length === 0) {
    return;
  }
  const vault = await client.request<{ secrets?: { name: string; size?: number }[] }>("/api/rudder/secrets", { method: "GET" }).catch(() => null);
  if (!vault) {
    return;
  }
  const stored = new Map((vault.secrets ?? []).map((secret) => [secret.name, secret.size ?? -1]));
  const pending: { relative: string; name: string; content: Buffer }[] = [];
  for (const relative of envFiles) {
    const content = await fsp.readFile(path.join(repoRoot, relative)).catch(() => null);
    if (!content || content.length > MAX_SECRET_VALUE_BYTES) {
      continue;
    }
    const name = `~/code/${repoName}/${relative}`;
    if (stored.get(name) === content.length) {
      continue;
    }
    pending.push({ relative, name, content });
  }
  if (pending.length === 0) {
    return;
  }
  const list = pending.map((entry) => entry.relative).join(", ");
  const verb = pending.some((entry) => stored.has(entry.name)) ? "Update" : "Copy";
  const yes = await promptConfirm(`${verb} ${list} in the cloud vault so agents on the home machine have it?`, true);
  if (!yes) {
    return;
  }
  for (const entry of pending) {
    await client.request<JsonValue>("/api/rudder/secrets/item", {
      method: "PUT",
      body: {
        name: entry.name,
        kind: "file",
        filePath: entry.name,
        valueBase64: entry.content.toString("base64"),
        source: "rudder cloud",
      },
    });
  }
  process.stderr.write(`Stored ${pending.length} file${pending.length === 1 ? "" : "s"} in the vault.\n`);
}

function appUrl(baseUrl: string): string {
  return `${baseUrl.replace(/\/$/, "")}/app`;
}

// Sign the phone in from this (already signed-in) laptop: a short, single-use
// code the Rudder app trades for a token on the same account. No OAuth.
async function printAppLink(options: CloudCommandOptions): Promise<void> {
  const client = await cloudClient({ requireToken: true });
  const pairing = await client.request<{ code: string; expiresAt: string; link: string; url: string }>(
    "/api/app/pair/start",
    { method: "POST", body: {} },
  );
  if (options.json) {
    printJson({ app: appUrl(client.baseUrl), ...pairing } as unknown as JsonValue);
    return;
  }
  console.log(`Pair your phone (code works once, for 10 minutes):

  Code:  ${pairing.code}

  iPhone app:  tap "Pair with this code" on the sign-in screen and type it,
               or open ${pairing.link}
  Browser:     ${pairing.url}`);
}

/**
 * `rudder cloud home runs <runId...>`: which of these agents (moved here from a
 * laptop) still exist on the home machine; `known: false` when it cannot
 * answer, which callers must read as "unknown", never "deleted".
 * `rudder cloud home delete <runId...>`: delete them there, through that repo's
 * dashboard, which also takes them off the phone. Queued if the machine is
 * asleep. This is what `dd` on a moved row in the laptop's dashboard runs.
 */
async function homeAgentsCommand(client: CloudClient, sub: "runs" | "delete", args: string[], options: CloudCommandOptions): Promise<void> {
  const yes = options.yes || args.includes("--yes") || args.includes("-y");
  const runIds = args.filter((arg) => !arg.startsWith("-"));
  if (runIds.length === 0) {
    throw new Error(`Usage: rudder cloud home ${sub} <runId...>`);
  }
  const repo = await githubSlugFromOrigin(findRepoRoot());
  if (!repo) {
    throw new Error("This repo has no GitHub origin, so none of its agents live on the home machine.");
  }
  if (sub === "delete" && !yes && !options.json) {
    if (!isTty()) {
      throw new Error("Refusing to delete agents without a terminal to confirm in; pass --yes.");
    }
    if (!(await promptConfirm(`Delete ${runIds.length === 1 ? "this agent" : `${runIds.length} agents`} on the home machine? This cannot be undone.`, false))) {
      console.log("Nothing deleted.");
      return;
    }
  }
  const result = await client.request<{ known?: boolean; existing?: string[]; status?: string }>("/api/rudder/home/agents", {
    method: "POST",
    body: { repo, action: sub === "runs" ? "check" : "delete", runIds },
  });
  if (options.json) {
    printJson(result as unknown as JsonValue);
    return;
  }
  if (sub === "runs") {
    if (!result.known) {
      console.log("The home machine is not connected, so it can't say.");
      return;
    }
    const existing = new Set(result.existing ?? []);
    for (const id of runIds) {
      console.log(`${existing.has(id) ? "there " : "gone  "} ${id}`);
    }
    return;
  }
  console.log(result.status === "queued"
    ? "The home machine is asleep; it deletes them as soon as it is back."
    : "Deleted on the home machine.");
}

async function homeCommand(args: string[], options: CloudCommandOptions): Promise<void> {
  const client = await cloudClient({ requireToken: true });
  const sub = args[0] ?? "status";
  // An agent on the home machine stopping it would kill itself and every other
  // agent there mid-task (and a "restart" from inside never gets to the start
  // half). Restarts come from a laptop or the phone.
  if (sub === "stop" && process.env.RUDDER_CLOUD_RUNTIME === "home" && !options.yes) {
    throw new Error("refusing to stop the home machine from inside it: that ends this session and every agent on it. Run it from your laptop or the phone (or pass --force).");
  }
  if (sub === "start" || sub === "stop") {
    const result = await client.request<JsonValue>(`/api/rudder/home/${sub}`, { method: "POST", body: {} });
    if (options.json) {
      printJson(result);
    } else {
      console.log(sub === "start" ? "Home machine starting." : "Home machine stopped.");
    }
    return;
  }
  if (sub === "region") {
    const region = (args[1] ?? "").trim().toLowerCase();
    if (!region) {
      throw new Error("Usage: rudder cloud home region <fly region, e.g. sjc>");
    }
    if (process.env.RUDDER_CLOUD_RUNTIME === "home") {
      throw new Error("run this from your laptop: moving the home machine restarts it.");
    }
    type Move = { from: string; to: string; state: string; oldVolumeId?: string; error?: string };
    const started = await client.request<{ move: Move }>("/api/rudder/home/region", { method: "POST", body: { region } });
    console.log(`Moving the home machine ${started.move.from} -> ${started.move.to}. Its disk is copied; the old one is kept as a backup.`);
    let last = "";
    for (let i = 0; i < 400; i += 1) {
      await sleep(3000);
      const { move } = await client.request<{ move: Move | null }>("/api/rudder/home/region", { method: "GET" });
      if (!move) {
        continue;
      }
      if (move.state !== last) {
        console.log(`  ${move.state}`);
        last = move.state;
      }
      if (move.state === "failed") {
        throw new Error(`the move failed: ${move.error ?? "unknown error"}`);
      }
      if (move.state === "done") {
        console.log(`Done. The old disk (${move.oldVolumeId ?? "?"}) in ${move.from} is untouched; delete it once everything checks out.`);
        return;
      }
    }
    console.log("Still moving; check `rudder cloud home` in a few minutes.");
    return;
  }
  if (sub === "open") {
    const repo = args[1];
    if (!repo || !/^[\w.-]+\/[\w.-]+$/.test(repo)) {
      throw new Error("Usage: rudder cloud home open <owner/repo>");
    }
    const result = await client.request<JsonValue>("/api/rudder/home/open", { method: "POST", body: { repo } });
    if (options.json) {
      printJson(result);
    } else {
      console.log(`Opened ${repo} on the home machine.`);
    }
    return;
  }
  if (sub === "runs" || sub === "delete") {
    await homeAgentsCommand(client, sub, args.slice(1), options);
    return;
  }
  if (sub !== "status") {
    throw new Error("Usage: rudder cloud home [status|start|stop|region <code>|open <owner/repo>|runs <runId...>|delete <runId...>]");
  }
  const result = await client.request<{ home: Record<string, JsonValue> | null; projects: Record<string, JsonValue>[] }>(
    "/api/rudder/home",
    { method: "GET" },
  );
  if (options.json) {
    printJson(result as unknown as JsonValue);
    return;
  }
  if (!result.home) {
    console.log("No home machine yet. Run `rudder cloud` in a repo with a GitHub origin to create it.");
    return;
  }
  const home = result.home;
  const tailnet = home.tailnet && typeof home.tailnet === "object" && !Array.isArray(home.tailnet)
    ? (home.tailnet as Record<string, JsonValue>)
    : null;
  console.log(`home ${home.id}  ${home.status}${home.connected ? "  connected" : ""}  ${home.activeAgents ?? 0} agent(s) working`);
  if (home.failureReason) {
    console.log(`why: ${home.failureReason}`);
  }
  if (tailnet) {
    console.log(`tailnet: ${tailnet.dnsName ?? tailnet.hostname ?? "?"}${tailnet.online ? "" : "  (offline)"}${tailnet.error ? `  ${tailnet.error}` : ""}`);
  }
  for (const project of result.projects) {
    const agents = Array.isArray(project.agents) ? (project.agents as Record<string, JsonValue>[]) : [];
    const working = agents.filter((agent) => agent.status === "working").length;
    const waiting = agents.filter((agent) => agent.status === "waiting" || agent.status === "review").length;
    console.log(`  ${project.name}  ${project.repo || "(local, rudder cloud attach " + project.name + ")"}  ${project.dashboard ?? project.status}  ${working} working, ${waiting} need you`);
    if (project.failureReason) {
      console.log(`    why: ${project.failureReason}`);
    }
    const ports = Array.isArray(project.ports) ? (project.ports as Record<string, JsonValue>[]) : [];
    for (const port of ports) {
      console.log(`    :${port.port} ${port.label ?? ""} ${port.url ?? ""}`);
    }
  }
  console.log(`Phone: ${appUrl(client.baseUrl)}`);
}
