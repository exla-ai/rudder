import path from "node:path";
import { classifyTaskRoute, type TaskRoutingResult } from "../task-routing.js";

export interface ExecutionSelection {
  checkoutPath: string;
  location: "local" | "cloud";
  workspaceMode?: "isolated" | "selected";
}
export interface TaskIngress {
  requestId: string;
  task: string;
  execution: ExecutionSelection;
  attachments: string[];
  conversationReferences: string[];
  selectedAgent?: { runId: string; runEpoch: number; status: "running" | "paused" | "stopped" | "completed" };
  control?: "interrupt" | "resume" | "review" | "open" | "settings";
}
export type IngressDecision = {
  ingress: TaskIngress;
  action: "new-task" | "follow-up" | "control";
  routing?: TaskRoutingResult;
  indicator: string;
};

/** Shared native/CLI/Cloud adapter. It proposes dispatch, never starts a worker.
 * References and checkout are local context, never classifier evidence. Selected
 * agent identity is resolved by the authenticated caller, not model text. */
export async function routeTaskIngress(
  request: TaskIngress,
  options: Parameters<typeof classifyTaskRoute>[1] = {},
): Promise<IngressDecision> {
  const ingress = structuredClone(request);
  validateIngress(ingress);
  const location = `${ingress.execution.location}: ${ingress.execution.checkoutPath}`;
  if (ingress.control) {
    return { ingress, action: "control", indicator: `${ingress.control} · ${location}` };
  }
  if (ingress.selectedAgent) {
    return { ingress, action: "follow-up", indicator: `Message to ${ingress.selectedAgent.runId} (${ingress.selectedAgent.status}) · ${location}` };
  }
  const routing = await classifyTaskRoute(ingress.task, options);
  return { ingress, action: "new-task", routing, indicator: `${routing.route} · ${location} · ${routing.reason}` };
}

export function validateIngress(ingress: TaskIngress): void {
  if (!ingress || typeof ingress !== "object" || typeof ingress.requestId !== "string" || !ingress.requestId.trim()
    || ingress.requestId.length > 200 || typeof ingress.task !== "string" || ingress.task.includes("\0")
    || (!ingress.control && !ingress.task.trim()) || Buffer.byteLength(ingress.task) > 64 * 1024) throw new Error("Invalid task ingress");
  if (ingress.execution?.workspaceMode && !["isolated", "selected"].includes(ingress.execution.workspaceMode)) throw new Error("Invalid execution workspace mode");
  if (!ingress.execution || !["local", "cloud"].includes(ingress.execution.location)
    || typeof ingress.execution.checkoutPath !== "string" || !path.isAbsolute(ingress.execution.checkoutPath)
    || ingress.execution.checkoutPath.includes("\0")) throw new Error("An explicit execution checkout and location are required");
  for (const refs of [ingress.attachments, ingress.conversationReferences]) {
    if (!Array.isArray(refs) || refs.length > 64 || refs.some(ref => typeof ref !== "string" || !ref.trim() || ref.length > 2048 || ref.includes("\0"))) throw new Error("Invalid task references");
  }
  if (ingress.selectedAgent && (typeof ingress.selectedAgent.runId !== "string" || !ingress.selectedAgent.runId.trim()
    || !Number.isSafeInteger(ingress.selectedAgent.runEpoch) || ingress.selectedAgent.runEpoch <= 0
    || !["running", "paused", "stopped", "completed"].includes(ingress.selectedAgent.status))) throw new Error("Invalid selected agent");
  if (ingress.control && !["interrupt", "resume", "review", "open", "settings"].includes(ingress.control)) throw new Error("Invalid typed control");
}
