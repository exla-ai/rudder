/** Isolated Unix-socket qualification only. Deliberately not exported from index.ts. */
import fs from "node:fs/promises";
import { AuthorityPool } from "./pool.js";
import { ProjectAuthority } from "./authority.js";
import { budgets, type PoolBudgets } from "./config.js";
import { fail, uuid } from "./types.js";
import { migrateAuthority } from "./migration.js";

export async function qualificationPool(options: PoolBudgets & { nonce: string; socket: string; user: string; database: string; authorityId: string; role: "runtime" | "migration" }): Promise<{ authority: ProjectAuthority; pool: AuthorityPool; migrate(runtimeRole: string): ReturnType<typeof migrateAuthority> }> {
  uuid(options.nonce); uuid(options.authorityId);
  if (!options.socket.startsWith("/var/tmp/rudder-authority-node-") || options.socket.includes("..")) fail("invalid_request");
  const marker = await fs.readFile(options.socket + "/../qualification-nonce", "utf8").catch(() => "");
  const stat = await fs.stat(options.socket);
  if (marker.trim() !== options.nonce || (stat.mode & 0o777) !== 0o700 || stat.uid !== process.getuid?.()) fail("forbidden");
  const pool = new AuthorityPool({ host: options.socket, port: 55432, user: options.user, password: "unused-private-socket", database: options.database,
    ssl: false, application_name: "rudder-authority-node-qualification" }, budgets(options), options.role);
  return { pool, authority: new ProjectAuthority(options.authorityId, pool), migrate: role => migrateAuthority(pool, { authorityId: options.authorityId, runtimeRole: role }) };
}
