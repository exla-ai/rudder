// Product analytics for the web/phone app: a whitelist of named events,
// forwarded to the same PostHog project as the CLI (src/analytics.ts). The
// distinct id is a sha256 prefix of the account id, never the id or an email,
// and `$process_person_profile: false` keeps events anonymous.
//
// Props are sanitized here, not trusted from the client: only primitives, at
// most 20 keys, and any string that looks like a path, a token or an email is
// dropped. A capture is fire-and-forget with a 3s timeout and never throws.
//
// Every account event carries `internal`: true for the team's own accounts
// (an exla.ai address or an admin), decided here on the server and never sent
// by the client, so dashboards can leave the team out.
//
// Before there is an account (first open, the sign-in screen) events are keyed
// by a random device id the app keeps, hashed here (DEVICE_EVENTS).
//
// Off with RUDDER_TELEMETRY=0, and under tests unless RUDDER_ANALYTICS_URL
// points somewhere (that also overrides the host, so tests can catch events).

import { createHash } from "node:crypto";
import type { IncomingMessage, ServerResponse } from "node:http";

export const PRODUCT_EVENTS = [
  "onboarding_viewed",
  "onboarding_surface_toggled",
  "onboarding_completed",
  "org_created",
  "org_invite_created",
  "org_invite_accepted",
  "org_member_role_changed",
  "org_member_removed",
  "integration_connect_clicked",
  "integration_connected",
  "integration_disconnected",
  "model_picked",
  "agent_panel_opened",
  "plan_panel_opened",
  "task_started",
  "download_mac_clicked",
  "install_cli_copied",
  "phone_link_opened",
  "chat_model_switched",
  "all_agents_started",
  // { feature }: a panel or tool opened (subagent tray, terminal, files...).
  // The server sends it too, for Tailscale.
  "feature_used",
  // Captured by the server itself (server.ts), whichever client acted:
  "app_active",
  "app_error",
  "message_sent",
  "message_edited",
  "prompt_answered",
  "agent_action",
  "agent_branched",
  "chat_harness_switched",
  "plan_approved",
  "agent_pulled",
  "voice_session_started",
  "voice_tool_used",
  "voice_call_started",
  "voice_call_ended",
  // The funnel after sign-in: an account signed in, its cloud machine came up,
  // an agent CLI was signed in on it, agents started there (createHomeWatch).
  "signed_in",
  "home_created",
  "home_ready",
  "agent_login_started",
  "agent_signed_in",
  "agent_started",
] as const;

export type ProductEventName = (typeof PRODUCT_EVENTS)[number];

const EVENT_SET = new Set<string>(PRODUCT_EVENTS);
// Counted by the server where the thing happens. A client posting one would
// only double-count it (or fake it), so /api/app/event refuses these.
export const SERVER_ONLY_EVENTS: ReadonlySet<string> = new Set([
  "app_active", "app_error", "message_sent", "message_edited", "prompt_answered", "agent_action", "agent_branched",
  "chat_harness_switched", "plan_approved", "agent_pulled", "voice_session_started", "voice_tool_used", "voice_call_started", "voice_call_ended",
  "signed_in", "home_created", "home_ready", "agent_login_started", "agent_signed_in", "agent_started",
]);
// Before sign-in, posted by the app to /api/app/anon-event with its device id.
export const DEVICE_EVENTS = ["app_first_opened", "signin_viewed", "signin_clicked", "signin_completed"] as const;
// Anonymous events: the device ones, plus signin_started, which the server
// sends when a Mac or CLI device login starts (keyed by that login).
export const ANONYMOUS_EVENTS = [...DEVICE_EVENTS, "signin_started"] as const;
const DEVICE_SET = new Set<string>(DEVICE_EVENTS);
const ANONYMOUS_SET = new Set<string>(ANONYMOUS_EVENTS);
export const INTERNAL_EMAIL_DOMAIN = "exla.ai";
// Same project as the CLI (src/analytics.ts): a write-only project token.
const POSTHOG_HOST = "https://us.i.posthog.com";
const POSTHOG_PROJECT_KEY = "phc_2KfOzM4nSxjAgPDfgEikP7Kp7JVcU5aJ0kAyEQykkmn";
const CAPTURE_TIMEOUT_MS = 3000;
const MAX_PROPS = 20;
const MAX_STRING = 200;
const DEVICE_ID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
// The anonymous endpoint needs no account, so each device gets a small budget.
const DEVICE_EVENTS_PER_HOUR = 30;

export type ProductProps = Record<string, string | number | boolean | null>;

export function isProductEvent(name: unknown): name is ProductEventName {
  return typeof name === "string" && EVENT_SET.has(name);
}

/** The team's own accounts: an exla.ai address, or one of the admins. */
export function isInternalEmail(email: string | null | undefined, adminEmails: ReadonlySet<string>): boolean {
  const lower = (email ?? "").trim().toLowerCase();
  return lower !== "" && (adminEmails.has(lower) || lower.endsWith(`@${INTERNAL_EMAIL_DOMAIN}`));
}

/** Keep primitives only, at most 20 keys; drop strings that look like a path,
 * a token, an email or a url with credentials. */
export function sanitizeProductProps(props: unknown): ProductProps {
  const clean: ProductProps = {};
  if (!props || typeof props !== "object" || Array.isArray(props)) {
    return clean;
  }
  let kept = 0;
  for (const [key, value] of Object.entries(props as Record<string, unknown>)) {
    if (kept >= MAX_PROPS) {
      break;
    }
    if (!/^[A-Za-z][A-Za-z0-9_]{0,39}$/.test(key)) {
      continue;
    }
    if (typeof value === "string") {
      const text = value.trim();
      if (
        text.startsWith("/") ||
        text.startsWith("~") ||
        /^[A-Za-z]:\\/.test(text) ||
        text.includes("\\") ||
        /\.\.?\//.test(text) ||
        /[^\s@]+@[^\s@]+\.[^\s@]+/.test(text) ||
        /(sk-|ghp_|gho_|github_pat_|phc_|rdr_|rdrw_|xox[abprs]-|lin_api_|whsec_|AKIA[0-9A-Z]{8})/.test(text) ||
        /^[A-Za-z0-9_\-+/=.]{32,}$/.test(text) ||
        /:\/\/[^/\s]*@/.test(text)
      ) {
        continue;
      }
      clean[key] = text.length > MAX_STRING ? text.slice(0, MAX_STRING) : text;
    } else if (typeof value === "number") {
      if (!Number.isFinite(value)) {
        continue;
      }
      clean[key] = value;
    } else if (typeof value === "boolean" || value === null) {
      clean[key] = value;
    } else {
      continue;
    }
    kept += 1;
  }
  return clean;
}

export function analyticsDistinctId(accountId: string): string {
  return createHash("sha256").update(`rudder-account:${accountId}`).digest("hex").slice(0, 24);
}

/** A device or one sign-in attempt, before there is an account. */
export function anonymousDistinctId(key: string): string {
  return `anon_${createHash("sha256").update(`rudder-anon:${key}`).digest("hex").slice(0, 24)}`;
}

type Env = Record<string, string | undefined>;
type FetchLike = (url: string, init: { method: string; headers: Record<string, string>; body: string; signal: AbortSignal }) => Promise<{ ok: boolean }>;

export type ProductAnalytics = ReturnType<typeof createProductAnalytics>;

export function createProductAnalytics(options: { env?: Env; fetch?: FetchLike; internal: (accountId: string) => boolean }) {
  const env = options.env ?? process.env;
  const doFetch: FetchLike | undefined = options.fetch ?? (globalThis.fetch as unknown as FetchLike | undefined);
  const override = (env.RUDDER_ANALYTICS_URL || "").trim();
  const telemetry = (env.RUDDER_TELEMETRY || "").trim().toLowerCase();
  const underTest = env.NODE_ENV === "test" || Boolean(env.NODE_TEST_CONTEXT);
  const key = (env.RUDDER_POSTHOG_KEY ?? POSTHOG_PROJECT_KEY).trim();
  const host = (override || env.RUDDER_POSTHOG_HOST || POSTHOG_HOST).trim().replace(/\/+$/, "");
  const enabled =
    Boolean(doFetch) &&
    Boolean(key) &&
    !["0", "false", "off", "no"].includes(telemetry) &&
    (!underTest || Boolean(override));
  const deviceBudget = new Map<string, { count: number; resetAt: number }>();

  async function send(event: string, distinctId: string, properties: Record<string, unknown>): Promise<boolean> {
    try {
      const controller = new AbortController();
      const timer = setTimeout(() => controller.abort(), CAPTURE_TIMEOUT_MS);
      try {
        const response = await doFetch!(`${host}/i/v0/e/`, {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ api_key: key, event, distinct_id: distinctId, properties, timestamp: new Date().toISOString() }),
          signal: controller.signal,
        });
        return Boolean(response?.ok);
      } finally {
        clearTimeout(timer);
      }
    } catch {
      return false;
    }
  }

  /** Send one event. Resolves true only when PostHog accepted it (for tests);
   * callers do not need to await it. Unknown names are ignored. */
  async function capture(accountId: string, name: string, props: unknown = {}): Promise<boolean> {
    if (!enabled || !isProductEvent(name) || !accountId) {
      return false;
    }
    let internal: boolean;
    try {
      internal = options.internal(accountId);
    } catch {
      return false;
    }
    return send(name, analyticsDistinctId(accountId), {
      ...sanitizeProductProps(props),
      internal,
      $process_person_profile: false,
      source: "app",
    });
  }

  /** An event with no account yet, keyed by a device id or a login attempt. */
  async function captureAnonymous(key: string, name: string, props: unknown = {}): Promise<boolean> {
    if (!enabled || !ANONYMOUS_SET.has(name) || !key) {
      return false;
    }
    return send(name, anonymousDistinctId(key), { ...sanitizeProductProps(props), $process_person_profile: false, source: "app" });
  }

  /** Route POST /api/app/event { name, props? } -> 204. Returns false for any
   * other path. Unknown event names are a 400. */
  async function handleEventApi(
    req: IncomingMessage,
    res: ServerResponse,
    url: URL,
    account: { accountId: string },
    deps: {
      sendJson: (res: ServerResponse, status: number, body: any) => void;
      readJsonBody: (req: IncomingMessage) => Promise<unknown>;
    },
  ): Promise<boolean> {
    if (url.pathname.replace(/\/+$/, "") !== "/api/app/event") {
      return false;
    }
    const record = await readEventBody(req, res, deps);
    if (!record) {
      return true;
    }
    if (!isProductEvent(record.name) || SERVER_ONLY_EVENTS.has(record.name)) {
      deps.sendJson(res, 400, { error: "unknown event" });
      return true;
    }
    void capture(account.accountId, record.name, record.props);
    res.writeHead(204);
    res.end();
    return true;
  }

  /** Route POST /api/app/anon-event { name, device, props? } -> 204, with no
   * account: the app before sign-in. `device` is the random id the app keeps. */
  async function handleAnonEventApi(
    req: IncomingMessage,
    res: ServerResponse,
    url: URL,
    deps: {
      sendJson: (res: ServerResponse, status: number, body: any) => void;
      readJsonBody: (req: IncomingMessage) => Promise<unknown>;
    },
  ): Promise<boolean> {
    if (url.pathname.replace(/\/+$/, "") !== "/api/app/anon-event") {
      return false;
    }
    const record = await readEventBody(req, res, deps);
    if (!record) {
      return true;
    }
    if (typeof record.name !== "string" || !DEVICE_SET.has(record.name)) {
      deps.sendJson(res, 400, { error: "unknown event" });
      return true;
    }
    if (typeof record.device !== "string" || !DEVICE_ID.test(record.device)) {
      deps.sendJson(res, 400, { error: "device must be a uuid" });
      return true;
    }
    const device = record.device.toLowerCase();
    const now = Date.now();
    let budget = deviceBudget.get(device);
    if (!budget || budget.resetAt <= now) {
      if (deviceBudget.size > 50_000) {
        deviceBudget.clear();
      }
      budget = { count: 0, resetAt: now + 60 * 60 * 1000 };
      deviceBudget.set(device, budget);
    }
    budget.count += 1;
    if (budget.count > DEVICE_EVENTS_PER_HOUR) {
      deps.sendJson(res, 429, { error: "too many events" });
      return true;
    }
    void captureAnonymous(device, record.name, record.props);
    res.writeHead(204);
    res.end();
    return true;
  }

  async function readEventBody(
    req: IncomingMessage,
    res: ServerResponse,
    deps: { sendJson: (res: ServerResponse, status: number, body: any) => void; readJsonBody: (req: IncomingMessage) => Promise<unknown> },
  ): Promise<Record<string, unknown> | null> {
    if ((req.method || "GET").toUpperCase() !== "POST") {
      deps.sendJson(res, 405, { error: "method not allowed" });
      return null;
    }
    let body: unknown;
    try {
      body = await deps.readJsonBody(req);
    } catch (error) {
      const status = (error as { status?: number }).status;
      deps.sendJson(res, status && status >= 400 ? status : 400, { error: status === 413 ? "payload too large" : "invalid JSON body" });
      return null;
    }
    return body && typeof body === "object" && !Array.isArray(body) ? (body as Record<string, unknown>) : {};
  }

  return { enabled, capture, captureAnonymous, handleEventApi, handleAnonEventApi };
}

// ---------------------------------------------------------------------------
// What happens on a cloud machine, from the state it publishes (home.mjs):
// each agent that appears (agent_started, with its harness and model), and
// each agent CLI sign-in that starts working (agent_signed_in). This counts
// agents started from any surface: the app, Slack, the dashboard or the CLI.
// In memory: after a restart the first state seeds what is known, counting
// only agents started in the last few minutes (the old process may have
// missed them) and, on a new machine, the sign-ins it came up with.
// ---------------------------------------------------------------------------

const HARNESSES = ["claude", "codex", "opencode", "antigravity"] as const;
const RECENT_AGENT_MS = 10 * 60 * 1000;
const NEW_HOME_MS = 60 * 60 * 1000;

export type WatchedHomeState = {
  projects: ReadonlyArray<{ coordinator?: unknown; agents?: unknown }>;
  credentials: Record<string, unknown> | null | undefined;
  /** When the cloud machine was created (ms). */
  homeCreatedAt: number;
};

export function createHomeWatch(capture: (accountId: string, name: ProductEventName, props: ProductProps) => void, now: () => number = Date.now) {
  const homes = new Map<string, { agents: Set<string>; logins: Set<string> | null }>();
  const text = (value: unknown): string | null => (typeof value === "string" && value.trim() ? value.trim() : null);

  return function noteHomeState(homeId: string, accountId: string, state: WatchedHomeState): void {
    const at = now();
    const known = homes.get(homeId);
    const seen = known ?? { agents: new Set<string>(), logins: null };
    if (!known) {
      if (homes.size > 20_000) {
        homes.clear();
      }
      homes.set(homeId, seen);
    }
    const projects = state.projects.filter((project) => project && typeof project === "object");
    const agentsOf = (project: { agents?: unknown }) =>
      (Array.isArray(project.agents) ? project.agents : []).filter(
        (agent): agent is Record<string, unknown> => Boolean(agent) && typeof agent === "object" && typeof (agent as Record<string, unknown>).id === "string",
      );
    const total = projects.reduce((sum, project) => sum + agentsOf(project).length, 0);
    const projectCount = projects.filter((project) => project.coordinator !== true).length;
    const current = new Set<string>();
    for (const project of projects) {
      for (const agent of agentsOf(project)) {
        const id = agent.id as string;
        current.add(id);
        if (seen.agents.has(id)) {
          continue;
        }
        const createdAt = typeof agent.createdAt === "number" ? agent.createdAt : Number.NaN;
        if (!known && !(at - createdAt < RECENT_AGENT_MS)) {
          continue;
        }
        capture(accountId, "agent_started", {
          harness: text(agent.backend) ?? "claude",
          model: text(agent.model) ?? "default",
          effort: text(agent.effort) ?? "auto",
          mode: text(agent.mode) ?? "default",
          account: agent.account === undefined || agent.account === null || agent.account === "main" ? "main" : "added",
          planned: typeof agent.planId === "string" && agent.planId !== "",
          all_agents: project.coordinator === true,
          agents: total,
          projects: projectCount,
        });
      }
    }
    // Agents deleted since drop out; an id never comes back.
    seen.agents = current;

    const credentials = state.credentials;
    if (!credentials || typeof credentials !== "object") {
      return;
    }
    const signedIn = new Map<string, { harness: string; account: string; plan: string | null; accounts: number }>();
    for (const harness of HARNESSES) {
      const rows = (Array.isArray(credentials[harness]) ? credentials[harness] : []) as unknown[];
      const ok = rows.filter(
        (row): row is Record<string, unknown> => Boolean(row) && typeof row === "object" && (row as Record<string, unknown>).ok === true && typeof (row as Record<string, unknown>).id === "string",
      );
      for (const row of ok) {
        signedIn.set(`${harness}:${row.id as string}`, { harness, account: row.id === "main" ? "main" : "added", plan: text(row.plan), accounts: ok.length });
      }
    }
    const announce = seen.logins !== null || at - state.homeCreatedAt < NEW_HOME_MS;
    seen.logins ??= new Set();
    for (const [key, login] of signedIn) {
      if (seen.logins.has(key)) {
        continue;
      }
      seen.logins.add(key);
      if (announce) {
        capture(accountId, "agent_signed_in", { harness: login.harness, account: login.account, plan: login.plan, accounts: login.accounts });
      }
    }
  };
}
