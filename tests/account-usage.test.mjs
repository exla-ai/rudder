import assert from "node:assert/strict";
import test from "node:test";
import {
  activeUsageLimit, accountReady, antigravityError, antigravityPoolOf, antigravityPools, antigravityUsageLimit, applyAccountChecks, checkedAccount,
  claudeUsageLimit, claudeUsageLimitText, codexUsageLimit, jsonEvents, quotaDetail, routingQuotaCheckFresh,
} from "../cloud/worker/account-usage.mjs";
import { handoffRefusal } from "../cloud/worker/handoff.mjs";
import { parseClaudeQuota, parseCodexQuota } from "../dist/usage.js";

test("provider usage caps are separate from auth failures, ordinary API errors and warnings", () => {
  const event = { type: "rate_limit_event", rate_limit_info: { status: "rejected", rateLimitType: "seven_day", resetsAt: 1700000000 } };
  const limit = { window: "weekly", resetsAt: 1700000000000 };
  assert.deepEqual(claudeUsageLimit(jsonEvents(`setup noise\n${JSON.stringify(event)}\n`)), limit);
  const withInfo = (updates) => [{ ...event, rate_limit_info: { ...event.rate_limit_info, ...updates } }];
  assert.equal(claudeUsageLimit(withInfo({ rateLimitType: "seven_day_opus" })), null);
  assert.equal(claudeUsageLimit(withInfo({ status: "allowed_warning", utilization: 0.98 })), null);
  assert.equal(claudeUsageLimit(withInfo({ overageStatus: "allowed" })), null);
  assert.equal(claudeUsageLimit([event, ...withInfo({ status: "allowed" })]), null);
  assert.equal(claudeUsageLimitText("You've used 90% of your weekly limit"), null);
  assert.equal(claudeUsageLimitText("You've hit your weekly limit for Opus"), null);
  assert.deepEqual(claudeUsageLimitText("You've hit your weekly limit · resets Nov 22 at 11pm"), { window: "weekly", resetsAt: null });

  const failure = (message) => [{ type: "turn.failed", error: { message } }];
  assert.deepEqual(codexUsageLimit(failure("You’ve hit your usage limit. Try again on Nov 22.")), { window: null, resetsAt: null });
  assert.equal(codexUsageLimit(failure("You’ve hit your usage limit for Spark. Switch to another model now.")), null);
  assert.equal(codexUsageLimit(failure("exceeded retry limit, last status: 429")), null);
  assert.equal(codexUsageLimit(failure("refresh token already used")), null);
  assert.equal(codexUsageLimit([{ type: "item.completed", item: { text: "You’ve hit your usage limit." } }]), null);
});

test("accurate account-wide meters supply reset times without rounding or model-specific false positives", () => {
  const window = { utilization: 100, resets_at: "2026-11-22T23:00:00.000Z" };
  const resetsAt = Date.parse(window.resets_at);
  const expected = { window: "weekly", resetsAt };
  assert.deepEqual(parseClaudeQuota({ seven_day: window }, null, null).usageLimit, expected);
  assert.deepEqual(parseClaudeQuota({ five_hour: { utilization: 100 }, seven_day: window }, null, null).usageLimit, { window: "session", resetsAt: null });
  assert.equal(parseClaudeQuota({ seven_day: { ...window, utilization: 99.99 } }, null, null).usageLimit, null);
  assert.equal(parseClaudeQuota({ seven_day_opus: window }, null, null).usageLimit, null);
  // A model's own spent limit leaves the account available and names the model.
  const fable = parseClaudeQuota({ seven_day: { utilization: 87 }, limits: [
    { is_active: true, percent: 100, resets_at: "2026-11-28T22:00:00Z", scope: { model: { display_name: "Fable" } } },
    { is_active: true, percent: 40, scope: { model: { display_name: "Opus" } } },
  ] }, null, null);
  assert.deepEqual([fable.usageLimit, fable.modelLimits], [null, [{ model: "Fable", resetsAt: Date.parse("2026-11-28T22:00:00Z") }]]);
  assert.equal(parseClaudeQuota({ seven_day: window, extra_usage: { is_enabled: true } }, null, null).usageLimit, null);
  const meter = { used_percent: 100, reset_at: resetsAt / 1000, limit_window_seconds: 604800 };
  assert.deepEqual(parseCodexQuota({ rate_limit: { limit_reached: true, primary_window: meter } }).usageLimit, expected);
  assert.equal(parseCodexQuota({ rate_limit: { limit_reached: false, primary_window: meter } }).usageLimit, null);
  assert.equal(parseCodexQuota({ rate_limit: { limit_reached: true, allowed: true, primary_window: meter } }).usageLimit, null);
});

test("a confirmed cap survives inconclusive checks and expires at the reset boundary", () => {
  const capped = (id, resetsAt) => ({ id, ok: false, signedIn: true, usageLimit: { window: "weekly", resetsAt } });
  const expired = { id: "expired", ok: false, signedIn: false };
  const failed = { id: "failed", ok: false, signedIn: true };
  const confirmed = capped("main", 1000);
  assert.equal(accountReady(confirmed, 999), false);
  assert.equal(accountReady(confirmed, 1000), true);
  assert.equal(accountReady(capped("capped", null), 999999), false);
  assert.equal(accountReady(expired, 999999), false);
  assert.equal(accountReady(failed, 999999), false);

  const temporarilyUnavailable = { ok: false, signedIn: true, detail: "service unavailable" };
  assert.deepEqual(activeUsageLimit(checkedAccount(confirmed, temporarilyUnavailable, 800), 800), confirmed.usageLimit);
  assert.equal(activeUsageLimit(checkedAccount(confirmed, { ok: false, signedIn: false }, 800), 800), null);
  assert.equal(activeUsageLimit(checkedAccount(confirmed, { ok: true }, 800), 800), null);
  assert.equal(activeUsageLimit(checkedAccount(confirmed, { ...temporarilyUnavailable, account: "another@example.com" }, 800), 800), null);
  assert.match(handoffRefusal({ claude: [capped("main", null)] }, "claude", "main", "Claude Code"), /weekly usage limit/);
  assert.equal(handoffRefusal({ claude: [confirmed] }, "claude", "main", "Claude Code"), null);
});

test("the routing probe is refreshed after more input or a provider failure", () => {
  const record = { lastUserInputAt: "1000", updatedAt: "1200" };
  assert.equal(routingQuotaCheckFresh(1100, record, 1300), true);
  assert.equal(routingQuotaCheckFresh(1100, { ...record, lastUserInputAt: "1150" }, 1300), false);
  assert.equal(routingQuotaCheckFresh(1100, { ...record, attention: "error" }, 1300), false);
  assert.equal(routingQuotaCheckFresh(1201, { ...record, attention: "error" }, 1300), true);
  assert.equal(routingQuotaCheckFresh(1100, record, 31100), false);
});

test("completed periodic probes preserve newer quota, sign-out and account changes", () => {
  const main = { id: "main", default: true, ok: true };
  const signedIn = { id: "work", default: false, ok: true };
  const signedOut = { id: "work", default: false, ok: false, signedIn: false };
  const removed = { id: "removed", default: false, ok: true };
  const capped = { ...main, ok: false, signedIn: true, usageLimit: { window: "weekly", resetsAt: null } };
  const checks = [main, signedIn, removed].map((previous) => ({ id: previous.id, previous, result: { ok: true } }));
  const configured = [{ id: "main", default: false }, { id: "work", default: true }];
  assert.deepEqual(applyAccountChecks(configured, [capped, signedOut, removed], checks), [
    { ...capped, default: false }, { ...signedOut, default: true },
  ]);
});

test("a spent Antigravity quota becomes a usage limit with its reset, not the raw AGY_ERROR line", () => {
  const now = 1_800_000_000_000;
  const agy = (fields) => `Loading...\nAGY_ERROR: ${JSON.stringify({ code_kind: "http", retryable: true, error_id: "e-1", ...fields })}\n`;
  const quota = antigravityError(agy({
    short_error: "RESOURCE_EXHAUSTED (code 429): Individual quota reached. Please upgrade your subscription to increase your limits. Resets in 2h29m59s.",
    status: "RESOURCE_EXHAUSTED", error_code: 429,
  }));
  assert.equal(quota.message, "Individual quota reached. Please upgrade your subscription to increase your limits. Resets in 2h29m59s.");
  const limit = antigravityUsageLimit(quota, now);
  assert.deepEqual(limit, { window: null, resetsAt: now + (2 * 3600 + 29 * 60 + 59) * 1000 });
  assert.equal(quotaDetail(limit), "Reached the usage limit");
  assert.deepEqual(antigravityUsageLimit({ ...quota, message: "Individual quota reached. Resets in 45s." }, now), { window: null, resetsAt: now + 45_000 });
  assert.deepEqual(antigravityUsageLimit({ ...quota, message: "Individual quota reached." }, now), { window: null, resetsAt: null });

  // Capacity errors and plain rate limits leave the account usable.
  const capacity = antigravityError(agy({ short_error: "UNAVAILABLE (code 503): No capacity available for model gemini-4-neon on the server", status: "UNAVAILABLE", error_code: 503 }));
  assert.equal(capacity.message, "No capacity available for model gemini-4-neon on the server");
  assert.equal(antigravityUsageLimit(capacity, now), null);
  assert.equal(antigravityUsageLimit(antigravityError(agy({ short_error: "RESOURCE_EXHAUSTED (code 429): Too many requests.", status: "RESOURCE_EXHAUSTED", error_code: 429 })), now), null);
  assert.equal(antigravityError("Error: something else"), null);
});

test("Antigravity's pools come from its quota report, and a pool is spent only until its reset", () => {
  const now = Date.parse("2026-11-24T00:00:00Z");
  const quota = { error: null, pools: [
    { name: "Gemini", spent: true, resetsAt: "2026-11-24T02:00:00Z" },
    { name: "Claude and GPT", spent: false, resetsAt: null },
    { name: "Gemini 4 Argon", spent: true, resetsAt: "2026-11-23T09:00:00Z" },
  ] };
  assert.deepEqual(antigravityPools(quota, now), [
    { name: "Gemini", spent: true, resetsAt: Date.parse("2026-11-24T02:00:00Z") },
    { name: "Claude and GPT", spent: false, resetsAt: null },
    { name: "Gemini 4 Argon", spent: false, resetsAt: null },
  ]);
  assert.equal(antigravityPools({ ...quota, error: "could not reach Google" }, now), null);
  assert.equal(antigravityPools(null, now), null);
});

test("an Antigravity model draws on its own pool: third-party models, a family's own pool, else the shared Gemini pool", () => {
  const pools = [{ name: "Gemini" }, { name: "Claude and GPT" }, { name: "Gemini 4 Argon" }];
  const pool = (model) => antigravityPoolOf(model, pools)?.name ?? null;
  assert.equal(pool("gemini-4-neon-xhigh"), "Gemini");
  assert.equal(pool("gemini-3.8-flash-low"), "Gemini");
  assert.equal(pool("gemini-pro-orchestrator-neon"), "Gemini");
  assert.equal(pool("gemini-4-argon-medium"), "Gemini 4 Argon");
  assert.equal(pool("gemini-pro-orchestrator-argon"), "Gemini 4 Argon");
  assert.equal(pool("claude-opus-5-5-low"), "Claude and GPT");
  assert.equal(pool("gpt-oss-120b-medium"), "Claude and GPT");
  assert.equal(antigravityPoolOf("gemini-4-neon-low", [{ name: "Claude and GPT" }]), null);
  assert.equal(antigravityPoolOf("something-else", pools), null);
});

test("a move onto an Antigravity model whose pool is used up is refused; the account's other models go ahead", () => {
  const resetsAt = Date.now() + 3_600_000;
  const credentials = { antigravity: [{ id: "main", ok: true, account: "a@b.c", modelPools: [
    { name: "Gemini", spent: true, resetsAt, models: ["gemini-4-neon-low"] },
    { name: "Claude and GPT", spent: false, resetsAt: null, models: ["claude-opus-5-5-low"] },
  ] }] };
  assert.match(handoffRefusal(credentials, "antigravity", "main", "Antigravity", "gemini-4-neon-low"), /^Gemini models are used up on this Antigravity account/);
  assert.equal(handoffRefusal(credentials, "antigravity", "main", "Antigravity", "claude-opus-5-5-low"), null);
  assert.equal(handoffRefusal(credentials, "antigravity", "main", "Antigravity"), null);
  credentials.antigravity[0].modelPools[0].resetsAt = Date.now() - 1;
  assert.equal(handoffRefusal(credentials, "antigravity", "main", "Antigravity", "gemini-4-neon-low"), null);
});
