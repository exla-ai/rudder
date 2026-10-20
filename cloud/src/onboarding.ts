// Onboarding: which surfaces an account means to use Rudder from (web, the Mac
// app, the terminal, the phone) and whether it has finished the first-run
// picker. The app reads it to decide what to show first; analytics reads the
// choice to learn which surfaces matter.
//
// The store is pure over a better-sqlite3 handle; handleOnboardingApi routes
// GET/POST /api/app/onboarding with injected helpers so this module never
// imports server.ts.

import type { IncomingMessage, ServerResponse } from "node:http";
import type Database from "better-sqlite3";

export type Surface = "web" | "mac" | "terminal" | "phone";

/** Canonical order: stored and returned in this order whatever was sent. */
export const SURFACES: readonly Surface[] = ["web", "mac", "terminal", "phone"];

export type OnboardingSummary = { surfaces: Surface[]; completedAt: string | null };

export class OnboardingError extends Error {
  status: number;
  constructor(status: number, message: string) {
    super(message);
    this.name = "OnboardingError";
    this.status = status;
  }
}

/** Validate, dedupe and order a surfaces list, or throw OnboardingError(400). */
export function normalizeSurfaces(value: unknown): Surface[] {
  if (!Array.isArray(value)) {
    throw new OnboardingError(400, "surfaces must be a list");
  }
  const picked = new Set<Surface>();
  for (const item of value) {
    if (typeof item !== "string" || !(SURFACES as readonly string[]).includes(item)) {
      throw new OnboardingError(400, `unknown surface: ${String(item).slice(0, 40)}`);
    }
    picked.add(item as Surface);
  }
  return SURFACES.filter((surface) => picked.has(surface));
}

function parseStored(raw: unknown): Surface[] {
  if (typeof raw !== "string") {
    return [];
  }
  try {
    const list = JSON.parse(raw);
    return Array.isArray(list) ? SURFACES.filter((surface) => list.includes(surface)) : [];
  } catch {
    return [];
  }
}

export type OnboardingStore = ReturnType<typeof createOnboardingStore>;

export function createOnboardingStore(database: Database.Database, options: { now?: () => number } = {}) {
  const now = options.now ?? (() => Date.now());
  database.exec(`
    create table if not exists rudder_onboarding (
      account_id text primary key,
      surfaces text not null default '[]',
      completed_at integer,
      updated_at integer not null
    );
  `);
  const get = database.prepare("select surfaces, completed_at from rudder_onboarding where account_id = ?");
  const upsert = database.prepare(`
    insert into rudder_onboarding (account_id, surfaces, completed_at, updated_at)
    values (@accountId, @surfaces, @completedAt, @updatedAt)
    on conflict(account_id) do update set
      surfaces = excluded.surfaces,
      completed_at = coalesce(rudder_onboarding.completed_at, excluded.completed_at),
      updated_at = excluded.updated_at
  `);

  function onboardingSummary(accountId: string): OnboardingSummary {
    const row = get.get(accountId) as { surfaces: string; completed_at: number | null } | undefined;
    return {
      surfaces: parseStored(row?.surfaces),
      completedAt: row?.completed_at ? new Date(row.completed_at).toISOString() : null,
    };
  }

  /** Apply a POST body. `complete: true` stamps completedAt once; a finished
   * onboarding cannot be un-finished (`complete: false` is a 400). */
  function updateOnboarding(accountId: string, input: { surfaces?: unknown; complete?: unknown }): OnboardingSummary {
    if (input.complete !== undefined && input.complete !== true) {
      if (input.complete === false) {
        throw new OnboardingError(400, "onboarding cannot be un-completed");
      }
      throw new OnboardingError(400, "complete must be true");
    }
    const current = onboardingSummary(accountId);
    const surfaces = input.surfaces === undefined ? current.surfaces : normalizeSurfaces(input.surfaces);
    const at = now();
    upsert.run({
      accountId,
      surfaces: JSON.stringify(surfaces),
      completedAt: input.complete === true ? at : null,
      updatedAt: at,
    });
    return onboardingSummary(accountId);
  }

  return { onboardingSummary, updateOnboarding };
}

export type OnboardingApiDeps = {
  store: OnboardingStore;
  sendJson: (res: ServerResponse, status: number, body: any) => void;
  readJsonBody: (req: IncomingMessage) => Promise<unknown>;
  capture?: (accountId: string, name: string, props?: Record<string, unknown>) => void;
};

/** Route GET/POST /api/app/onboarding. Returns false for any other path. */
export async function handleOnboardingApi(
  req: IncomingMessage,
  res: ServerResponse,
  url: URL,
  account: { accountId: string },
  deps: OnboardingApiDeps,
): Promise<boolean> {
  if (url.pathname.replace(/\/+$/, "") !== "/api/app/onboarding") {
    return false;
  }
  const method = (req.method || "GET").toUpperCase();
  try {
    if (method === "GET") {
      deps.sendJson(res, 200, deps.store.onboardingSummary(account.accountId));
      return true;
    }
    if (method !== "POST") {
      deps.sendJson(res, 405, { error: "method not allowed" });
      return true;
    }
    let raw: unknown;
    try {
      raw = await deps.readJsonBody(req);
    } catch (error) {
      if ((error as { status?: number }).status) {
        throw error;
      }
      throw new OnboardingError(400, "invalid JSON body");
    }
    const body = raw && typeof raw === "object" && !Array.isArray(raw) ? (raw as Record<string, unknown>) : {};
    const before = deps.store.onboardingSummary(account.accountId);
    const after = deps.store.updateOnboarding(account.accountId, body);
    if (!before.completedAt && after.completedAt) {
      try {
        const props: Record<string, unknown> = { surface_count: after.surfaces.length };
        for (const surface of SURFACES) {
          props[`surface_${surface}`] = after.surfaces.includes(surface);
        }
        deps.capture?.(account.accountId, "onboarding_completed", props);
      } catch {
        // analytics never breaks a request
      }
    }
    deps.sendJson(res, 200, after);
    return true;
  } catch (error) {
    if (error instanceof OnboardingError) {
      deps.sendJson(res, error.status, { error: error.message });
      return true;
    }
    throw error;
  }
}
