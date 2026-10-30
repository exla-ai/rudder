import { exact, fail, uuid } from "./types.js";

export interface PoolBudgets {
  maxConnections?: number;
  maxQueued?: number;
  connectMs?: number;
  requestMs?: number;
  statementMs?: number;
  lockMs?: number;
  idleTransactionMs?: number;
}
export interface PostgresAuthorityConfig extends PoolBudgets {
  mode: "postgres";
  authorityId: string;
  connection: { host: string; port: number; database: string; user: string; password: string; ca?: string };
}
export type AuthorityConfig = { mode: "inactive" } | PostgresAuthorityConfig;
export interface Budgets { maxConnections: number; maxQueued: number; connectMs: number; requestMs: number; statementMs: number; lockMs: number; idleTransactionMs: number; }
export function budgets(input: PoolBudgets): Budgets {
  const defaults: Budgets = { maxConnections: 4, maxQueued: 16, connectMs: 750, requestMs: 2000, statementMs: 1500, lockMs: 250, idleTransactionMs: 2000 };
  const limits: Record<keyof Budgets, [number, number]> = { maxConnections: [1, 16], maxQueued: [0, 128], connectMs: [10, 5000], requestMs: [50, 15000], statementMs: [10, 10000], lockMs: [1, 2000], idleTransactionMs: [50, 15000] };
  for (const key of Object.keys(defaults) as (keyof Budgets)[]) {
    const value = input[key] ?? defaults[key]; const [min, max] = limits[key];
    if (!Number.isInteger(value) || value < min || value > max) fail("invalid_request");
    defaults[key] = value;
  }
  if (defaults.lockMs > defaults.statementMs) fail("invalid_request");
  return defaults;
}
export function validateConfig(config: PostgresAuthorityConfig): void {
  if (config.mode !== "postgres") fail("invalid_request");
  exact(config, ["mode", "authorityId", "connection", "maxConnections", "maxQueued", "connectMs", "requestMs", "statementMs", "lockMs", "idleTransactionMs"]);
  uuid(config.authorityId);
  const c = config.connection;
  if (!c || Object.keys(c).some(key => !["host", "port", "database", "user", "password", "ca"].includes(key))
    || [c.host,c.database,c.user,c.password].some(value => typeof value !== "string" || !value || value.includes("\0") || value.length > 65536)
    || !/^[a-zA-Z0-9.-]+$/.test(c.host) || c.host.length > 253
    || !Number.isInteger(c.port) || c.port < 1 || c.port > 65535
    || (c.ca !== undefined && (typeof c.ca !== "string" || !c.ca.includes("BEGIN CERTIFICATE") || c.ca.length > 256 * 1024))) fail("invalid_request");
  budgets(config);
}
