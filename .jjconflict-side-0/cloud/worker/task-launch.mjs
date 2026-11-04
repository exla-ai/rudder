const ROUTING_UNAVAILABLE = "Rudder Cloud could not load or run the packaged task router. No implementation worker was started.";
const INVALID_ROUTING = "Rudder Cloud received an invalid task-routing result. No implementation worker was started.";

/**
 * Legacy sails finish when the finite `rudder <backend>` command finishes.
 * Until they have a native dashboard lifecycle adapter, discussion and gated
 * planning must use the home workspace instead of an implementation contract.
 * This resolver is per supervisor startup, not per WebSocket connection.
 */
export function createWorkerLaunchResolver({
  isWorkspaceMode,
  task,
  backend,
  env,
  loadRouting = () => import("/opt/rudder/dist/task-routing.js"),
}) {
  let resolved;
  return () => {
    resolved ??= resolveLaunch();
    return resolved;
  };

  async function resolveLaunch() {
    // Existing workspace/handoff dashboards already own their imported runs.
    // Do not reinterpret a copied task or inject it into those sessions again.
    if (isWorkspaceMode || !task) return { command: "rudder", args: [], route: null };
    let decision;
    try {
      const routing = await loadRouting();
      if (typeof routing?.classifyTaskRoute !== "function") throw new Error("router missing");
      decision = await routing.classifyTaskRoute(task, { env });
    } catch {
      // An old/broken image must not silently default a read-only request to
      // execution. Do not include upstream errors or task secrets in the report.
      throw new Error(ROUTING_UNAVAILABLE);
    }
    if (!decision || typeof decision !== "object"
      || !["discuss", "execute", "plan"].includes(decision.route)
      || !["deterministic", "decisions"].includes(decision.source)
      || typeof decision.reason !== "string" || decision.reason.length > 256
      || /[\u0000-\u001f\u007f]/u.test(decision.reason)) {
      throw new Error(INVALID_ROUTING);
    }
    if (decision.route !== "execute") {
      const scope = decision.route === "discuss" ? "discussion or read-only planning" : "approval-gated multi-agent planning";
      throw new Error(`This alpha's legacy Cloud task sails cannot safely start ${scope}. No implementation worker was started. Submit this task in the project's Cloud home workspace using the web or phone app.`);
    }
    return { command: "rudder", args: [backend, "--worktree", task], route: decision };
  }
}
