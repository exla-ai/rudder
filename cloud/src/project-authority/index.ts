import { budgets, validateConfig, type AuthorityConfig, type PostgresAuthorityConfig } from "./config.js";
import { AuthorityPool } from "./pool.js";
import { ProjectAuthority } from "./authority.js";
import { migrateAuthority } from "./migration.js";
import { exact, fail } from "./types.js";

function pool(config: PostgresAuthorityConfig, role: "runtime" | "migration"): AuthorityPool {
  validateConfig(config);
  const c = config.connection;
  return new AuthorityPool({ host: c.host, port: c.port, database: c.database, user: c.user, password: c.password,
    ssl: { rejectUnauthorized: true, servername: c.host, ...(c.ca ? { ca: c.ca } : {}) }, application_name: "rudder-project-authority" }, budgets(config), role);
}
export function createAuthority(config: AuthorityConfig = { mode: "inactive" }): ProjectAuthority {
  if (config.mode === "inactive") { exact(config, ["mode"]); return new ProjectAuthority("00000000-0000-0000-0000-000000000000"); }
  if (config.mode !== "postgres") fail("invalid_request");
  return new ProjectAuthority(config.authorityId, pool(config, "runtime"));
}
/** Explicit administrator connection, distinct from the runtime API role. Never called during factory creation. */
export function createAuthorityMigrator(config: PostgresAuthorityConfig): { migrate(runtimeRole: string): ReturnType<typeof migrateAuthority>; close(): Promise<void> } {
  const manager = pool(config, "migration");
  return { migrate: runtimeRole => migrateAuthority(manager, { authorityId: config.authorityId, runtimeRole }), close: () => manager.close() };
}
export { AuthorityError } from "./types.js";
export { MonotonicLeaseFence } from "./lease.js";
export type { ProjectAuthority } from "./authority.js";
export type { AuthorityConfig, PostgresAuthorityConfig, PoolBudgets } from "./config.js";
export type { Accepted, Outcome, TrustedContext, OwnerGrant, OwnerFence, RootAdmission, HandoffAdmission,
  BootstrapRequest, AcquireRequest, RenewRequest, ReleaseRequest, DecisionRequest, VerificationAdmission, IntegrationRequest } from "./types.js";
