// The provider's subscription allowance and its credentials are independent.
const CLAUDE_WINDOWS = new Map([
  ["five_hour", "session"],
  ["seven_day", "weekly"],
  ["seven_day_overage_included", "weekly"],
]);

export function jsonEvents(text) {
  return text.split("\n").flatMap((line) => {
    try {
      return [JSON.parse(line)];
    } catch {
      return [];
    }
  });
}

function resetTime(epochSeconds) {
  return Number.isFinite(epochSeconds) && epochSeconds > 0 && epochSeconds < 8_640_000_000_000
    ? epochSeconds * 1000
    : null;
}

export function claudeUsageLimit(events) {
  const limit = events.findLast((event) => event?.type === "rate_limit_event")?.rate_limit_info;
  if (limit?.status !== "rejected" || limit.isUsingOverage || limit.overageInUse
      || limit.overageStatus === "allowed" || limit.overageStatus === "allowed_warning") return null;
  const window = CLAUDE_WINDOWS.get(limit.rateLimitType);
  if (window) return { window, resetsAt: resetTime(limit.resetsAt) };
  return null;
}

export function claudeUsageLimitText(detail) {
  if (typeof detail !== "string") return null;
  // Claude Code's own messages. Warnings and model-specific limits don't
  // block the entire account.
  const described = /^you['’]ve hit your (?:(weekly|session) )?limit(?:[. ·]|$)/i.exec(detail);
  if (described && !/^you['’]ve hit your (?:weekly |session )?limit for /i.test(detail)) {
    return { window: described[1]?.toLowerCase() ?? null, resetsAt: null };
  }
  return /^usage limit reached[.!]?$/i.test(detail) ? { window: null, resetsAt: null } : null;
}

// Codex exec --json exposes the CLI's final provider error in turn.failed.
// Do not treat a generic 429, a retry error, or the output of an agent as a cap.
export function codexUsageLimit(events) {
  const detail = events.findLast((event) => event?.type === "turn.failed")?.error?.message;
  return typeof detail === "string" && /^you['’]ve hit your usage limit\./i.test(detail)
    ? { window: null, resetsAt: null }
    : null;
}

// agy prints a failed request as one line:
//   AGY_ERROR: {"short_error":"RESOURCE_EXHAUSTED (code 429): Individual quota
//   reached. Please upgrade your subscription to increase your limits. Resets
//   in 2h29m59s.","status":"RESOURCE_EXHAUSTED","error_code":429,...}
// `message` is the short error without its "STATUS (code N): " prefix.
export function antigravityError(text) {
  const line = /^AGY_ERROR: (\{.*\})$/m.exec(text);
  if (!line) return null;
  const error = JSON.parse(line[1]);
  const short = String(error.short_error ?? "");
  return { status: error.status ?? null, code: error.error_code ?? null, message: short.replace(/^[A-Z_]+ \(code \d+\): /, "") || short };
}

// A spent Antigravity quota ("Individual quota reached") with the reset agy
// gives as a duration. A capacity error or a plain rate limit is not one.
export function antigravityUsageLimit(error, now = Date.now()) {
  if (error?.status !== "RESOURCE_EXHAUSTED" || !/\bquota reached\b/i.test(error.message)) return null;
  const reset = /\bresets in (?:(\d+)h)?(?:(\d+)m)?(?:(\d+)s)?/i.exec(error.message);
  const [, h, m, s] = reset ?? [];
  const ms = h || m || s ? ((Number(h ?? 0) * 60 + Number(m ?? 0)) * 60 + Number(s ?? 0)) * 1000 : null;
  return { window: null, resetsAt: ms === null ? null : now + ms };
}

// Antigravity meters its models in pools, each with its own allowance
// (src/usage.ts parseAntigravityQuota names them as agy does). The pools from
// its quota report: [{ name, spent, resetsAt }], resetsAt in milliseconds or
// null; a pool whose reset has passed is no longer spent. Null when the report
// has no pools to go by.
export function antigravityPools(quota, now = Date.now()) {
  if (!quota || quota.error || !quota.pools?.length) return null;
  return quota.pools.map((pool) => {
    const resetsAt = pool.spent && pool.resetsAt !== null ? Date.parse(pool.resetsAt) : null;
    const spent = pool.spent && (resetsAt === null || resetsAt > now);
    return { name: pool.name, spent, resetsAt: spent ? resetsAt : null };
  });
}

export function activeUsageLimit(account, now = Date.now()) {
  const limit = account?.usageLimit;
  return limit && (limit.resetsAt === null || limit.resetsAt > now) ? limit : null;
}

export function routingQuotaCheckFresh(checkedAt, record, now = Date.now()) {
  // The last send can have been rejected after a probe still showed capacity.
  // Native records store both input and lifecycle times in epoch milliseconds.
  const inputAt = Number(record.lastUserInputAt) || 0;
  const failedAt = record.attention === "error" ? Number(record.updatedAt) || 0 : 0;
  return now - checkedAt < 30_000 && checkedAt > Math.max(inputAt, failedAt);
}

export function accountReady(account, now = Date.now()) {
  return !activeUsageLimit(account, now) && (account?.ok === true || account?.signedIn === true && account.usageLimit != null);
}

// Keep the confirmed limit when a later probe is inconclusive. A successful
// request or actual logout replaces the status immediately.
export function checkedAccount(previous, result, now = Date.now()) {
  const confirmed = activeUsageLimit(previous, now);
  if (!confirmed || result.ok || !result.signedIn || result.account !== previous.account) return result;
  const next = result.usageLimit;
  const usageLimit = next?.resetsAt != null && (confirmed.resetsAt === null || next.resetsAt >= confirmed.resetsAt) ? next : confirmed;
  return { ...result, usageLimit };
}

export function applyAccountChecks(configured, latest, checks) {
  return configured.flatMap(({ id, default: preferred }) => {
    const current = latest.find((row) => row.id === id);
    const check = checks.find((probe) => probe.id === id);
    if (!check || current !== check.previous) {
      return current ? [{ ...current, default: preferred }] : [];
    }
    return [{ ...checkedAccount(current, check.result), id, default: preferred }];
  });
}

export function quotaDetail(limit) {
  return limit.window ? `Reached the ${limit.window} usage limit` : "Reached the usage limit";
}
