import pg, { type PoolConfig, type PoolClient, type QueryResult, type QueryResultRow } from "pg";
import { performance } from "node:perf_hooks";
import { AuthorityError, fail, type Outcome } from "./types.js";
import { type Budgets } from "./config.js";

export const SCHEMA = "rudder_project_authority";
export const RESTRICTED_ROLE_CHECK = `SELECT r.rolsuper OR r.rolcreatedb OR r.rolcreaterole OR r.rolreplication OR r.rolbypassrls
  OR pg_catalog.pg_has_role(r.oid,n.nspowner,'MEMBER') OR pg_catalog.has_schema_privilege(r.oid,n.oid,'CREATE')
  OR pg_catalog.has_database_privilege(r.oid,current_database(),'CREATE') OR pg_catalog.has_schema_privilege(r.oid,'public','CREATE')
  OR EXISTS(SELECT 1 FROM pg_catalog.pg_roles elevated WHERE (elevated.rolsuper OR elevated.rolcreatedb OR elevated.rolcreaterole
    OR elevated.rolreplication OR elevated.rolbypassrls) AND pg_catalog.pg_has_role(r.oid,elevated.oid,'MEMBER')) AS unsafe
  FROM pg_catalog.pg_roles r CROSS JOIN pg_catalog.pg_namespace n
  WHERE n.nspname=$1 AND r.rolname=COALESCE($2::name,current_user)`;
export function safeError(error: unknown): AuthorityError {
  if (error instanceof AuthorityError) return error;
  const code = typeof error === "object" && error !== null && "code" in error ? String(error.code) : "";
  return new AuthorityError(code === "57014" ? "deadline_exceeded" : code === "55P03" ? "busy"
    : code === "22003" ? "counter_exhausted" : code === "23505" ? "conflict" : "unavailable");
}
export interface Transaction { query<R extends QueryResultRow = any>(sql: string, values?: unknown[]): Promise<QueryResult<R>>; }

/** Internal pool. No arbitrary-query API or driver object is exported from index.ts. */
export class AuthorityPool {
  #pool: pg.Pool;
  #active = 0;
  #queue: { accept: () => void; reject: () => void; timer: NodeJS.Timeout }[] = [];
  #closed = false;
  constructor(config: PoolConfig, readonly limits: Budgets, readonly role: "runtime" | "migration") {
    this.#pool = new pg.Pool({ ...config, max: limits.maxConnections, connectionTimeoutMillis: limits.connectMs,
      idleTimeoutMillis: 10000, maxLifetimeSeconds: 300, allowExitOnIdle: true, query_timeout: 0,
      keepAlive: true, keepAliveInitialDelayMillis: 1000,
      options: `-c statement_timeout=${limits.statementMs} -c lock_timeout=${limits.lockMs} -c idle_in_transaction_session_timeout=${limits.idleTransactionMs} -c timezone=UTC`,
      types: { getTypeParser: (oid: number, format?: any) => [20, 1114, 1184].includes(oid) ? (value: string) => value : pg.types.getTypeParser(oid, format) },
    });
    // Driver errors may contain credentials/SQL. Never forward or log them.
    this.#pool.on("error", () => {});
    this.#pool.on("connect", client => { client.on("error", () => {}); });
  }
  stats(): { active: number; queued: number } { return { active: this.#active, queued: this.#queue.length }; }
  async #permit(deadline: number): Promise<() => void> {
    if (this.#closed) fail("inactive");
    if (this.#active < this.limits.maxConnections) this.#active++;
    else {
      if (this.#queue.length >= this.limits.maxQueued) fail("busy");
      await new Promise<void>((resolve, reject) => {
        const entry = { accept: resolve, reject: () => reject(new AuthorityError("deadline_exceeded")), timer: undefined as unknown as NodeJS.Timeout };
        entry.timer = setTimeout(() => { const index = this.#queue.indexOf(entry); if (index >= 0) this.#queue.splice(index, 1); entry.reject(); }, Math.max(0, deadline - performance.now()));
        this.#queue.push(entry);
      });
    }
    let released = false;
    return () => {
      if (released) return; released = true;
      const waiting = this.#queue.shift();
      if (waiting) { clearTimeout(waiting.timer); waiting.accept(); } else this.#active--;
    };
  }
  async run<T>(account: string, body: (tx: Transaction) => Promise<T>): Promise<Outcome<T>> {
    const deadline = performance.now() + this.limits.requestMs;
    const permit = await this.#permit(deadline);
    let client: PoolClient | undefined; let released = false; let begun = false; let committing = false; let expired = false;
    const destroy = () => { if (client && !released) { released = true; client.release(true); } };
    const timer = setTimeout(() => { expired = true; destroy(); }, Math.max(0, deadline - performance.now()));
    try {
      client = await this.#pool.connect();
      if (expired || performance.now() >= deadline) { destroy(); fail("deadline_exceeded"); }
      if (this.role === "runtime") {
        const check = await client.query(RESTRICTED_ROLE_CHECK, [SCHEMA, null]);
        if (check.rows.length !== 1 || check.rows[0].unsafe) fail("role_rejected");
      }
      await client.query("BEGIN ISOLATION LEVEL READ COMMITTED"); begun = true;
      await client.query("SELECT set_config('rudder.account_id',$1,true),set_config('synchronous_commit','on',true),set_config('search_path','pg_catalog',true)", [account]);
      const connection = client;
      const tx: Transaction = { query: async <R extends QueryResultRow = any>(sql: string, values?: unknown[]) => {
        const remaining = Math.floor(deadline - performance.now());
        if (expired || remaining <= 0) fail("deadline_exceeded");
        await connection.query("SELECT set_config('statement_timeout',$1,true),set_config('lock_timeout',$2,true)", [String(Math.max(1, Math.min(remaining, this.limits.statementMs))), String(Math.max(1, Math.min(remaining, this.limits.lockMs)))]);
        return connection.query<R>(sql, values);
      } };
      const value = await body(tx);
      if (expired || performance.now() >= deadline) fail("deadline_exceeded");
      committing = true;
      await client.query("COMMIT"); begun = false; committing = false;
      return { status: "committed", value };
    } catch (error) {
      if (committing) { destroy(); return { status: "uncertain", reconcile: "retry_same_operation" }; }
      if (client && begun && !released) {
        try { await client.query("ROLLBACK"); begun = false; } catch { destroy(); }
      }
      if (expired) throw new AuthorityError("deadline_exceeded");
      throw safeError(error);
    } finally {
      clearTimeout(timer);
      if (client && !released) { released = true; client.release(); }
      permit();
    }
  }
  async close(): Promise<void> {
    this.#closed = true;
    for (const entry of this.#queue.splice(0)) { clearTimeout(entry.timer); entry.reject(); }
    await this.#pool.end();
  }
}
