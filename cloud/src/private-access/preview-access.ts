/** The relay's authorization boundary. Worker reports are scoped by the
 * authenticated reporting connection, never by tenant fields in the payload.
 * Receipt ages use this process's monotonic clock, not a remote timestamp. */
export type PreviewScope = { tenantId: string; projectId: string; workspaceId: string; attemptId: string };
export type PreviewReport = PreviewScope & {
  id: string; deviceId: string; dnsName: string; url: string; port: number;
  // protocol describes the development listener. Serve can terminate HTTPS in
  // front of an HTTP listener, so url.protocol need not equal this field.
  pid: number; processStart: string; protocol: "http" | "https"; ready: true;
};
const matches = (a: PreviewScope, b: PreviewScope) => a.tenantId === b.tenantId && a.projectId === b.projectId
  && a.workspaceId === b.workspaceId && a.attemptId === b.attemptId;
const key = (scope: PreviewScope) => JSON.stringify([scope.tenantId, scope.projectId, scope.workspaceId, scope.attemptId]);

export class AuthorizedPreviewReports<Actor> {
  private readonly reports = new Map<string, { scope: PreviewScope; receivedAt: number; rows: PreviewReport[] }>();
  constructor(private readonly authorize: (actor: Actor, scope: PreviewScope) => Promise<boolean>, private readonly options: {
    now?: () => number; maxAgeMs?: number; maxReports?: number; timeoutMs?: number;
  } = {}) {}
  private now() { return (this.options.now ?? (() => performance.now()))(); }
  private async authorized(actor: Actor, scope: PreviewScope): Promise<boolean> {
    let timer: ReturnType<typeof setTimeout> | undefined;
    try {
      return await Promise.race([
        Promise.resolve().then(() => this.authorize(actor, scope)).catch(() => false),
        new Promise<false>((resolve) => { timer = setTimeout(() => resolve(false), this.options.timeoutMs ?? 1500); }),
      ]);
    } finally { if (timer) clearTimeout(timer); }
  }
  /** reportingScope MUST come from the authenticated worker/session record.
   * Call retire on worker loss/attempt replacement before accepting a new one. */
  receive(reportingScope: PreviewScope, reports: unknown) {
    const reportKey = key(reportingScope);
    if (!this.reports.has(reportKey) && this.reports.size >= (this.options.maxReports ?? 1024)) this.reports.delete(this.reports.keys().next().value!);
    const rows = (Array.isArray(reports) ? reports : []).slice(0, 64).filter((value): value is PreviewReport => {
      if (!value || typeof value !== "object" || Array.isArray(value)) return false;
      const row = value as Partial<PreviewReport>;
      if (![row.tenantId, row.projectId, row.workspaceId, row.attemptId, row.deviceId, row.processStart, row.dnsName, row.url, row.id]
        .every((field) => typeof field === "string" && field.length > 0 && !field.includes("\0"))) return false;
      const report = row as PreviewReport;
      if (!matches(report, reportingScope) || row.ready !== true || !/^[a-f0-9]{64}$/.test(report.id)
        || !row.deviceId || !row.processStart || !Number.isSafeInteger(row.pid) || report.pid <= 0
        || !Number.isInteger(row.port) || report.port < 1024 || report.port > 65535 || row.port === 41641
        || !["http", "https"].includes(report.protocol)) return false;
      try {
        const url = new URL(report.url);
        const host = report.dnsName.toLowerCase().replace(/\.$/, "");
        return /^[a-z0-9-]+\.[a-z0-9.-]+\.ts\.net$/.test(host) && ["http:", "https:"].includes(url.protocol)
          && url.hostname === host && Number(url.port) === row.port && url.pathname === "/"
          && !url.username && !url.password && !url.search && !url.hash;
      } catch { return false; }
    }).map((row) => ({ ...row }));
    this.reports.set(reportKey, { scope: { ...reportingScope }, receivedAt: this.now(), rows });
  }
  retire(scope: PreviewScope) { this.reports.delete(key(scope)); }
  async list(actor: Actor, scope: PreviewScope): Promise<PreviewReport[]> {
    if (!(await this.authorized(actor, scope))) throw new Error("Private preview access denied.");
    const report = this.reports.get(key(scope));
    const age = report ? this.now() - report.receivedAt : Infinity;
    if (!report || age < 0 || age > (this.options.maxAgeMs ?? 10_000)) return [];
    return report.rows.map((row) => ({ ...row }));
  }
  async resolve(actor: Actor, scope: PreviewScope, id: string): Promise<PreviewReport> {
    const row = (await this.list(actor, scope)).find((candidate) => candidate.id === id);
    if (!row) throw new Error("The project preview is no longer ready. Refresh discovery.");
    return row;
  }
}
