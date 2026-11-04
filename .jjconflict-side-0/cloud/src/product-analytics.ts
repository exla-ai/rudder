// Product analytics for the web/phone app: a whitelist of named events,
// forwarded to the same PostHog project as the CLI (src/analytics.ts). The
// distinct id is a sha256 prefix of the account id, never the id or an email,
// and `$process_person_profile: false` keeps events anonymous.
//
// Props are sanitized here, not trusted from the client: only primitives, at
// most 20 keys, and any string that looks like a path, a token or an email is
// dropped. A capture is fire-and-forget with a 3s timeout and never throws.
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
  "linear_issue_started",
  // Captured by the server itself (server.ts), whichever client acted:
  "app_active",
  "app_error",
  "message_sent",
  "prompt_answered",
  "agent_action",
  "plan_approved",
  "agent_pulled",
  "voice_session_started",
  "voice_tool_used",
  "voice_call_started",
  "voice_call_ended",
] as const;

export type ProductEventName = (typeof PRODUCT_EVENTS)[number];

const EVENT_SET = new Set<string>(PRODUCT_EVENTS);
// Counted by the server where the thing happens. A client posting one would
// only double-count it (or fake it), so /api/app/event refuses these.
export const SERVER_ONLY_EVENTS: ReadonlySet<string> = new Set([
  "app_active", "app_error", "message_sent", "prompt_answered", "agent_action", "plan_approved",
  "agent_pulled", "voice_session_started", "voice_tool_used", "voice_call_started", "voice_call_ended",
]);
// Same project as the CLI (src/analytics.ts): a write-only project token.
const POSTHOG_HOST = "https://us.i.posthog.com";
const POSTHOG_PROJECT_KEY = "phc_2KfOzM4nSxjAgPDfgEikP7Kp7JVcU5aJ0kAyEQykkmn";
const CAPTURE_TIMEOUT_MS = 3000;
const MAX_PROPS = 20;
const MAX_STRING = 200;

export type ProductProps = Record<string, string | number | boolean | null>;

export function isProductEvent(name: unknown): name is ProductEventName {
  return typeof name === "string" && EVENT_SET.has(name);
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

type Env = Record<string, string | undefined>;
type FetchLike = (url: string, init: { method: string; headers: Record<string, string>; body: string; signal: AbortSignal }) => Promise<{ ok: boolean }>;

export type ProductAnalytics = ReturnType<typeof createProductAnalytics>;

export function createProductAnalytics(options: { env?: Env; fetch?: FetchLike } = {}) {
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

  /** Send one event. Resolves true only when PostHog accepted it (for tests);
   * callers do not need to await it. Unknown names are ignored. */
  async function capture(accountId: string, name: string, props: unknown = {}): Promise<boolean> {
    try {
      if (!enabled || !isProductEvent(name) || !accountId) {
        return false;
      }
      const payload = {
        api_key: key,
        event: name,
        distinct_id: analyticsDistinctId(accountId),
        properties: { ...sanitizeProductProps(props), $process_person_profile: false, source: "app" },
        timestamp: new Date().toISOString(),
      };
      const controller = new AbortController();
      const timer = setTimeout(() => controller.abort(), CAPTURE_TIMEOUT_MS);
      try {
        const response = await doFetch!(`${host}/i/v0/e/`, {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify(payload),
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
    if ((req.method || "GET").toUpperCase() !== "POST") {
      deps.sendJson(res, 405, { error: "method not allowed" });
      return true;
    }
    let body: unknown;
    try {
      body = await deps.readJsonBody(req);
    } catch (error) {
      const status = (error as { status?: number }).status;
      deps.sendJson(res, status && status >= 400 ? status : 400, { error: status === 413 ? "payload too large" : "invalid JSON body" });
      return true;
    }
    const record = body && typeof body === "object" && !Array.isArray(body) ? (body as Record<string, unknown>) : {};
    if (!isProductEvent(record.name) || SERVER_ONLY_EVENTS.has(record.name)) {
      deps.sendJson(res, 400, { error: "unknown event" });
      return true;
    }
    void capture(account.accountId, record.name, record.props);
    res.writeHead(204);
    res.end();
    return true;
  }

  return { enabled, capture, handleEventApi };
}
