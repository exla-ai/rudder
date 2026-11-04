import { createHash } from "node:crypto";

/** Evidence references are opaque. Never persist transcript locations or commands. */
export function evidenceId(value: string): string {
  return createHash("sha256").update(value).digest("hex");
}

/** Defense in depth for bounded advisory text. Raw traces stay with the adapter. */
export function redact(value: string, limit = 512): string {
  const safe = value
    .replace(/-----BEGIN [^-]*PRIVATE KEY-----[\s\S]*?(?:-----END [^-]*PRIVATE KEY-----|$)/g, "[REDACTED]")
    .replace(/\b(?:Bearer|Basic)\s+[^\s,;]+/gi, "[REDACTED]")
    .replace(/\b(?:sk-[\w-]+|gh[pousr]_[\w]+|github_pat_[\w]+|xox[baprs]-[\w-]+|AKIA[A-Z0-9]{16})\b/g, "[REDACTED]")
    .replace(/([\w.-]*(?:token|secret|password|passwd|api[_-]?key|credential)[\w.-]*["']?\s*[:=]\s*)(?:"[^"\n]*"|'[^'\n]*'|[^\s&,;]+)/gi, "$1[REDACTED]")
    .replace(/(https?:\/\/)[^\s/@]+:[^\s/@]+@/gi, "$1[REDACTED]@")
    .replace(/\b[A-Z0-9._%+-]+@[A-Z0-9.-]+\.[A-Z]{2,}\b/gi, "[REDACTED]")
    .replace(/(?:\/Users\/|\/home\/)[^/\s]+/g, "/[PRIVATE_HOME]")
    .replace(/[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f]/g, "");
  return safe.length > limit ? `${safe.slice(0, Math.max(0, limit - 1))}…` : safe;
}

export function finiteMetric(value: unknown): number | undefined {
  return typeof value === "number" && Number.isFinite(value) && value >= 0 ? value : undefined;
}

export function label(value: unknown): string | undefined {
  if (typeof value !== "string" || !value.length || value.length > 96) return undefined;
  const safe = redact(value, 96);
  return safe === value && /^[\w./:-]+$/.test(safe) ? safe : undefined;
}
