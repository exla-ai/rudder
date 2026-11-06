import { queryPreviewOwner, type RegisteredPreviewProcess } from "./engine.js";
import { PrivatePreviewDirectory, observeLinuxPreviewListeners, previewProcessMatches, type PreviewListener, type PrivatePreview } from "./previews.js";

type Project = { workspaceId: string; dir: string };
/** One Home-wide directory owns Serve reconciliation. Only native App receipts
 * supply preview roles; ordinary agent listeners remain private and unpublished. */
export class HomePrivatePreviews {
  #projects = new Map<string, RegisteredPreviewProcess[]>();
  #roots = new Map<string,string>();
  #scanning?: Promise<void>;
  constructor(readonly tenantId: string, readonly directory = new PrivatePreviewDirectory(),
    readonly query = queryPreviewOwner, readonly observe = observeLinuxPreviewListeners) {}
  reconcile(projects: Project[]): Promise<void> {
    if (this.#scanning) return this.#scanning;
    return this.#scanning = this.#reconcile(projects).finally(() => { this.#scanning = undefined; });
  }
  async #reconcile(projects: Project[]): Promise<void> {
    const next = new Map<string, RegisteredPreviewProcess[]>();
    // One native owner snapshot at a time. Each spawn forks the Home supervisor,
    // which also relays every terminal: about 5 ms of blocked event loop at its
    // size, so four at once held keystrokes and frames for ~20 ms per batch,
    // ~100 ms per scan with 22 projects. A disappeared owner clears its reports
    // in this tick rather than preserving stale published URLs.
    for (const project of projects.slice(0, 64)) {
      try {
        const rows = await this.query(project.dir, "previews");
        if (Array.isArray(rows)) next.set(project.workspaceId, rows as RegisteredPreviewProcess[]);
      } catch { /* no current custody, so no preview */ }
    }
    this.#projects = next;
    this.#roots = new Map(projects.slice(0,64).map(project=>[project.workspaceId,project.dir]));
    const processes = [...next.values()].flat().map(row => ({tenantId:this.tenantId,
      projectId:row.scope.projectId,workspaceId:row.scope.workspaceId,attemptId:row.scope.attemptId,
      workspacePath:row.workspacePath,pid:row.pid,processStart:row.processStart,protocol:row.protocol,role:row.role}));
    await this.directory.reconcile(await this.observe(processes));
  }
  async processMatches(item: PreviewListener): Promise<boolean> {
    for (const [workspace, records] of this.#projects) {
      const prior = records.find(row => row.scope.projectId === item.projectId && row.scope.workspaceId === item.workspaceId
        && row.scope.attemptId === item.attemptId && row.pid === item.pid && row.processStart === item.processStart);
      const repo = this.#roots.get(workspace);
      if (!prior || !repo || item.tenantId !== this.tenantId) continue;
      try {
        // Probes and Serve commands can outlive an owner observation. Recheck
        // actual App custody before and after readiness, including owner epoch.
        const current = await this.query(repo,"previews",undefined,AbortSignal.timeout(1500));
        return Array.isArray(current) && current.some((row: RegisteredPreviewProcess) =>
          row.pid === prior.pid && row.processStart === prior.processStart && row.protocol === prior.protocol
          && row.workspacePath === prior.workspacePath && row.scope.projectId === prior.scope.projectId
          && row.scope.workspaceId === prior.scope.workspaceId && row.scope.attemptId === prior.scope.attemptId && row.scope.ownerEpoch === prior.scope.ownerEpoch
          && row.owner.owner_id === prior.owner.owner_id && row.owner.epoch === prior.owner.epoch)
          && await previewProcessMatches(item);
      } catch { return false; }
    }
    return false;
  }
  snapshot(workspaceId: string): PrivatePreview[] {
    const rows = this.#projects.get(workspaceId) ?? [];
    return this.directory.snapshot().filter(item => rows.some(row => row.scope.projectId === item.projectId
      && row.scope.workspaceId === item.workspaceId && row.scope.attemptId === item.attemptId
      && row.pid === item.pid && row.processStart === item.processStart));
  }
  async register(project: Project, request: unknown): Promise<unknown> {
    // This records an already-owned process. It cannot start a process or grant
    // custody to a project inferred from a path.
    return this.query(project.dir, "preview-register", request);
  }
}
