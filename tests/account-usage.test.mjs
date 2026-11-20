import assert from "node:assert/strict";
import test from "node:test";
import {
  activeUsageLimit, accountReady, checkedAccount, claudeUsageLimit,
  claudeUsageLimitText, codexUsageLimit, jsonEvents,
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
