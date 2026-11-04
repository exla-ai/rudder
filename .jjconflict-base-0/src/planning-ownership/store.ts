import { isDeepStrictEqual } from "node:util";
import { assertLegacyWriterAllowed } from "../engine-authority.js";
import { updateGraph } from "../graph.js";
import {
  admitOwnedPlan, approveOwnedPlan, replanOwnedTask,
  type PlanningOwner, type PlanningApproval, type PlanningGraph, type OwnedPlan, type PlanFence, type ReplanCommand,
} from "./index.js";

export type OwnedPlanningCommand =
  | { kind: "admit"; input: Parameters<typeof admitOwnedPlan>[1] }
  | { kind: "approve"; input: PlanFence }
  | { kind: "replan"; input: ReplanCommand };

/** The existing headless scheduler is the only legacy writer. Projector UI
 * forwards commands to that writer. Fabric projects must use the engine's
 * transactional command/receipt path, never this JSON projection adapter. */
export async function applyOwnedPlanningCommand(
  repoRoot: string,
  context: { mode: "scheduler" | "projector"; owner: PlanningOwner; approval: PlanningApproval },
  command: OwnedPlanningCommand,
): Promise<OwnedPlan> {
  if (context.mode !== "scheduler") throw new Error("Projector surfaces must forward planning commands to the project owner");
  assertLegacyWriterAllowed(repoRoot);
  const actual = context.owner, requested = command.input.owner;
  if (actual.project_id !== requested.project_id || actual.owner_id !== requested.owner_id || actual.epoch !== requested.epoch) {
    throw new Error("Planning command does not match the authenticated project owner");
  }
  let result: OwnedPlan | undefined;
  await updateGraph(repoRoot, graph => {
    // The grant comes from authenticated owner state, separately from the
    // command payload. A caller cannot approve its own larger budget/scope.
    const requestedGrant = command.kind === "admit" ? command.input.approval
      : (graph as PlanningGraph).planningOwnership?.[command.input.taskId]?.approval;
    if (!requestedGrant || !isDeepStrictEqual(requestedGrant, context.approval)) {
      throw new Error("Planning scope, budget or contract versions differ from the current owner grant");
    }
    result = command.kind === "admit" ? admitOwnedPlan(graph, command.input)
      : command.kind === "approve" ? approveOwnedPlan(graph, command.input) : replanOwnedTask(graph, command.input);
  });
  return result!;
}
