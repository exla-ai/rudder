/** Access-only snapshot. No refresh, external CLI lookup, store write or token
 * validity claim. Only the original Codex server authenticates this bearer. */
import type { AuthProfileCredential } from "./types.js";

export const MAX_CODEX_ACCESS_TOKEN_BYTES = 48 * 1024;
export const MAX_CODEX_ACCESS_SNAPSHOT_BYTES = 65532;
const MAX_FUTURE_MS = 24 * 60 * 60 * 1000;
export type CodexOAuthAccessSnapshotV1 = {
  formatVersion: 1; kind: "CodexOAuthAccessSnapshotV1"; profileId: string;
  backend: "codex"; provider: "openai-codex"; accessToken: string;
  chatgptAccountId: string; expiresAtMs: number; chatgptPlanType?: string;
};
export class CodexAccessSnapshotError extends Error {
  constructor() { super("The selected Codex OAuth access snapshot is unavailable or expired."); }
}
const fail = (): never => { throw new CodexAccessSnapshotError(); };
const text = (value: unknown, bound: number): value is string => typeof value === "string"
  && value.length > 0 && Buffer.byteLength(value) <= bound && !/[\p{Cc}\uD800-\uDFFF]/u.test(value);

/** JWT claims are unverified deadline/selection DATA. Ignore the store's
 * expires value, which legacy import can estimate from file timestamps. */
export function codexOAuthAccessSnapshot(profileId: string, credential: AuthProfileCredential,
  nowMs: number = Date.now()): CodexOAuthAccessSnapshotV1 {
  if (!text(profileId, 256) || !Number.isSafeInteger(nowMs) || nowMs < 0
    || credential.type !== "oauth" || credential.provider !== "openai-codex"
    || !text(credential.accountId, 256) || !text(credential.access, MAX_CODEX_ACCESS_TOKEN_BYTES)) return fail();
  const parts = credential.access.split(".");
  if (parts.length !== 3 || parts.some(part => !/^[A-Za-z0-9_-]+$/.test(part))) fail();
  let claims: Record<string, unknown>;
  try {
    const bytes = Buffer.from(parts[1], "base64url");
    try {
      if (bytes.length > 16 * 1024 || bytes.toString("base64url") !== parts[1]) fail();
      const value: unknown = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(bytes));
      if (!value || typeof value !== "object" || Array.isArray(value)) fail();
      claims = value as Record<string, unknown>;
    } finally { bytes.fill(0); }
  } catch { return fail(); }
  const exp = claims.exp;
  const auth = claims["https://api.openai.com/auth"];
  if (!Number.isSafeInteger(exp) || (exp as number) <= 0
    || !Number.isSafeInteger((exp as number) * 1000)
    || (exp as number) * 1000 <= nowMs
    || (exp as number) * 1000 - nowMs > MAX_FUTURE_MS
    || !auth || typeof auth !== "object" || Array.isArray(auth)) fail();
  const fields = auth as Record<string, unknown>;
  if (fields.chatgpt_account_id !== credential.accountId) fail();
  const plan = fields.chatgpt_plan_type;
  if (plan !== undefined && plan !== null && !text(plan, 128)) fail();
  return { formatVersion: 1, kind: "CodexOAuthAccessSnapshotV1", profileId,
    backend: "codex", provider: "openai-codex", accessToken: credential.access,
    chatgptAccountId: credential.accountId!, expiresAtMs: (exp as number) * 1000,
    ...(typeof plan === "string" ? { chatgptPlanType: plan } : {}) };
}
