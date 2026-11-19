/**
 * Several signed-in accounts per agent CLI. An account is a config dir the CLI
 * officially supports moving: `CLAUDE_CONFIG_DIR` for Claude Code, `CODEX_HOME`
 * for Codex.
 *
 * - "main" is the CLI's own dir (`~/.claude`, `~/.codex`, or what the env
 *   already points them at). Agents on it run with the env unchanged.
 * - Every other account is `<rudder home>/accounts/<backend>/<id>/`, and its
 *   agents run with the CLI's variable set to that dir (native signals.rs).
 *
 * An extra dir holds its own sign-in and links the rest to the main dir: the
 * user's settings, memory, skills and hooks, and the conversation logs. So a
 * chat started on one account resumes on another, and everything that reads
 * transcripts, plans or usage keeps reading the main dir only.
 *
 * The default account of a backend (what new agents start on) is
 * `backends.<backend>.account` in config.json; absent means main.
 *
 * Every function takes the env it resolves paths from: the Cloud home's own
 * process has RUDDER_HOME set to a flag, not a dir (cloud/worker/home.mjs), so
 * it passes the agents' env.
 */
import fs from "node:fs";
import fsp from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { defaultConfig } from "./state.js";
import type { JsonValue } from "./types.js";
import { readJson, updateJson } from "./util.js";

export type AccountBackend = "claude" | "codex";
export const ACCOUNT_BACKENDS: readonly AccountBackend[] = ["claude", "codex"];
export const MAIN_ACCOUNT = "main";

export type AgentAccount = {
  backend: AccountBackend;
  /** "main", or the extra account's id. */
  id: string;
  dir: string;
  /** New agents on this backend start here. */
  default: boolean;
};

type Env = NodeJS.ProcessEnv;

/** The variable each CLI reads its config dir from. */
export const ACCOUNT_ENV: Record<AccountBackend, string> = {
  claude: "CLAUDE_CONFIG_DIR",
  codex: "CODEX_HOME",
};

const ACCOUNT_ID = /^[a-z0-9]{1,32}$/;

/**
 * What an extra dir links to the main dir, and nothing else: anything not
 * named here is the account's own, so a file a newer CLI adds starts out
 * separate. Dirs are created in the main dir when missing, so the CLI's first
 * write lands in the shared one; files are linked when they exist.
 *
 * None of these holds a sign-in. What does, as of the versions the image pins
 * (claude 2.1.328, codex 0.170.0), stays in each account's own dir:
 * - Claude Code: `.credentials.json` (on macOS the keychain entry named after
 *   the dir, src/usage.ts claudeKeychainService), `.claude.json` (oauthAccount,
 *   userID, primaryApiKey, customApiKeyResponses and the org caches),
 *   `backups` (copies of it), `mcp-needs-auth-cache.json`, `mcp-oauth-tokens*.json`.
 * - Codex: `auth.json`, `auth.lock`, `auth.json.lock`, `.credentials.json` (MCP
 *   OAuth), `gateway_oauth.json`, `gateway_oauth.lock`, `installation_id`, the
 *   app-server daemon dirs and the sqlite state (the keyring entry is keyed by
 *   the dir already).
 */
const SHARED: Record<AccountBackend, { dirs: string[]; files: string[] }> = {
  claude: {
    dirs: ["projects", "plans", "tasks", "teams", "todos", "file-history", "session-env", "shell-snapshots", "skills", "commands", "agents", "plugins", "hooks", "rules", "output-styles"],
    files: ["settings.json", "CLAUDE.md", "AGENTS.md", "keybindings.json", "history.jsonl"],
  },
  codex: {
    dirs: ["sessions", "archived_sessions", "skills", "rules", "prompts"],
    files: ["config.toml", "AGENTS.md", "hooks.json", "history.jsonl", ".hook-trust"],
  },
};

/**
 * The parts of the main `.claude.json` a new Claude account starts with: the
 * user's choices (onboarding done, theme, MCP servers, folder trust), never
 * the signed-in account or its caches.
 */
const CLAUDE_STATE_KEYS = [
  "hasCompletedOnboarding",
  "theme",
  "bypassPermissionsModeAccepted",
  "remoteDialogSeen",
  "mcpServers",
  "projects",
  "autoCompactEnabled",
  "editorMode",
  "verbose",
  "installMethod",
  "autoUpdates",
];

export function isAccountBackend(value: unknown): value is AccountBackend {
  return value === "claude" || value === "codex";
}

function rudderHomeOf(env: Env): string {
  return path.resolve(env.RUDDER_HOME?.trim() || path.join(os.homedir(), ".rudder"));
}

function configPath(env: Env): string {
  return path.join(rudderHomeOf(env), "config.json");
}

/** The CLI's own config dir under `env`: the main account. */
export function mainAccountDir(backend: AccountBackend, env: Env): string {
  const configured = env[ACCOUNT_ENV[backend]]?.trim();
  return path.resolve(configured || path.join(os.homedir(), backend === "claude" ? ".claude" : ".codex"));
}

function checkId(id: string): void {
  if (id === MAIN_ACCOUNT || !ACCOUNT_ID.test(id)) {
    throw new Error(`not an account id: ${JSON.stringify(id)}`);
  }
}

function extraDir(backend: AccountBackend, id: string, env: Env): string {
  checkId(id);
  return path.join(rudderHomeOf(env), "accounts", backend, id);
}

export function accountDir(backend: AccountBackend, id: string, env: Env): string {
  return id === MAIN_ACCOUNT ? mainAccountDir(backend, env) : extraDir(backend, id, env);
}

const BACKEND_NAME: Record<AccountBackend, string> = { claude: "Claude Code", codex: "Codex" };

function requireExtra(backend: AccountBackend, id: string, env: Env): string {
  const dir = extraDir(backend, id, env);
  if (!fs.statSync(dir, { throwIfNoEntry: false })?.isDirectory()) {
    throw new Error(`There is no ${BACKEND_NAME[backend]} account ${id}.`);
  }
  return dir;
}

/** The env an agent on this account runs with, added to `env`. */
export function accountEnv(backend: AccountBackend, id: string, env: Env): Env {
  if (id === MAIN_ACCOUNT) {
    return { ...env };
  }
  return { ...env, [ACCOUNT_ENV[backend]]: requireExtra(backend, id, env) };
}

type BackendShape = { [key: string]: JsonValue | undefined };
type ConfigShape = { [key: string]: unknown; backends?: Record<string, BackendShape | undefined> };

function defaultIdOf(config: ConfigShape | null, backend: AccountBackend): string {
  const id = config?.backends?.[backend]?.account;
  return typeof id === "string" && id ? id : MAIN_ACCOUNT;
}

/** Every account of every backend, main first, then the others oldest first. */
export async function listAccounts(env: Env): Promise<Record<AccountBackend, AgentAccount[]>> {
  const config = await readJson<ConfigShape>(configPath(env));
  const out = {} as Record<AccountBackend, AgentAccount[]>;
  for (const backend of ACCOUNT_BACKENDS) {
    const root = path.join(rudderHomeOf(env), "accounts", backend);
    let entries: fs.Dirent[] = [];
    try {
      entries = await fsp.readdir(root, { withFileTypes: true });
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error;
    }
    // Ids are base-36 creation times of one length, so name order is age order.
    const ids = entries.filter((entry) => entry.isDirectory() && ACCOUNT_ID.test(entry.name) && entry.name !== MAIN_ACCOUNT)
      .map((entry) => entry.name)
      .sort();
    const chosen = defaultIdOf(config, backend);
    out[backend] = [MAIN_ACCOUNT, ...ids].map((id) => ({
      backend,
      id,
      dir: accountDir(backend, id, env),
      default: id === chosen,
    }));
  }
  return out;
}

/**
 * A new, signed-out account: its dir with the shared parts linked to the main
 * dir. The caller signs it in (the CLI's own login with `accountEnv`) and
 * removes it again if that does not finish.
 */
export async function createAccount(backend: AccountBackend, env: Env): Promise<AgentAccount> {
  const id = Date.now().toString(36);
  const dir = extraDir(backend, id, env);
  const main = mainAccountDir(backend, env);
  await fsp.mkdir(path.dirname(dir), { recursive: true, mode: 0o700 });
  // Not recursive: an id already taken is an error, never a shared dir.
  await fsp.mkdir(dir, { mode: 0o700 });
  try {
    await fsp.mkdir(main, { recursive: true });
    for (const name of SHARED[backend].dirs) {
      await fsp.mkdir(path.join(main, name), { recursive: true });
      await fsp.symlink(path.join(main, name), path.join(dir, name));
    }
    for (const name of SHARED[backend].files) {
      if (fs.existsSync(path.join(main, name))) {
        await fsp.symlink(path.join(main, name), path.join(dir, name));
      }
    }
    if (backend === "claude") {
      await seedClaudeState(dir, env);
    }
  } catch (error) {
    await fsp.rm(dir, { recursive: true, force: true });
    throw error;
  }
  const config = await readJson<ConfigShape>(configPath(env));
  return { backend, id, dir, default: defaultIdOf(config, backend) === id };
}

async function seedClaudeState(dir: string, env: Env): Promise<void> {
  // Claude keeps `.claude.json` in its config dir only when CLAUDE_CONFIG_DIR
  // names one; otherwise it is ~/.claude.json.
  const configured = env.CLAUDE_CONFIG_DIR?.trim();
  const mainState = configured ? path.join(path.resolve(configured), ".claude.json") : path.join(os.homedir(), ".claude.json");
  const state = (await readJson<Record<string, unknown>>(mainState)) ?? {};
  const seed: Record<string, unknown> = {};
  for (const key of CLAUDE_STATE_KEYS) {
    if (key in state) seed[key] = state[key];
  }
  await fsp.writeFile(path.join(dir, ".claude.json"), `${JSON.stringify(seed, null, 2)}\n`, { mode: 0o600 });
}

/**
 * Deletes an extra account's dir (its links go, never what they point at). New
 * agents that started on it move back to main as the default. Sign it out
 * first: on macOS the CLI's keychain entry outlives the dir.
 */
export async function removeAccount(backend: AccountBackend, id: string, env: Env): Promise<void> {
  const dir = requireExtra(backend, id, env);
  await fsp.rm(dir, { recursive: true });
  await updateJson<ConfigShape>(configPath(env), (config) => {
    if (!config || defaultIdOf(config, backend) !== id) return undefined;
    const { account: _removed, ...rest } = config.backends?.[backend] ?? {};
    return { ...config, backends: { ...config.backends, [backend]: rest } } as JsonValue;
  });
}

/** Makes `id` the account new agents on `backend` start on. */
export async function setDefaultAccount(backend: AccountBackend, id: string, env: Env): Promise<void> {
  if (id !== MAIN_ACCOUNT) {
    requireExtra(backend, id, env);
  }
  await updateJson<ConfigShape>(configPath(env), (config) => {
    // A machine nothing has configured yet starts from the defaults, as the
    // dashboard's own first write does (native config.rs).
    const base = (config ?? defaultConfig()) as ConfigShape;
    const { account: _old, ...rest } = base.backends?.[backend] ?? {};
    const next = id === MAIN_ACCOUNT ? rest : { ...rest, account: id };
    return { ...base, backends: { ...base.backends, [backend]: next } } as JsonValue;
  });
}
