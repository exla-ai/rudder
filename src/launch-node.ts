/**
 * The native dashboard's workspace helpers. index.ts routes them here before
 * loading main.js, so each call imports only the jj and warm snapshot code:
 * every task launch waits on `__launch-node`, and loading the whole CLI took
 * 73 ms of Node startup where this takes about 27.
 *
 * - `__launch-node --repo <root> --task <text> --node <run id> [--base <change>]`
 *   isolates a worker in a jj workspace: it colocates jj with the Git repo,
 *   creates the run's workspace (a fork of the warm snapshot when there is
 *   one) and prints one JSON line {path, workspaceName, jjChangeId}. Errors go
 *   to stderr with a non-zero exit, and the dashboard shows them.
 * - `__warm-start --repo <root>`: the dashboard opened this project. Starts the
 *   detached warm builder when the base has no snapshot yet, so the first task
 *   finds one ready instead of waiting for its build. Prints nothing.
 */
import { parseArgs } from "node:util";
import { colocateRefusal, createNodeWorkspace, ensureColocated, ensureJj, isJjRepo } from "./jj.js";
import { warmStart } from "./warm-snapshot.js";

export async function runLaunchNodeCommand(argv: string[]): Promise<void> {
  const { values } = parseArgs({
    args: argv,
    options: { repo: { type: "string" }, task: { type: "string" }, node: { type: "string" }, base: { type: "string" } },
    strict: true,
  });
  if (!values.repo) throw new Error("__launch-node requires --repo");
  if (!values.task) throw new Error("__launch-node requires --task");
  if (!values.node) throw new Error("__launch-node requires --node");
  ensureJj();
  await ensureColocated(values.repo);
  const workspace = await createNodeWorkspace({
    repoRoot: values.repo,
    runId: values.node,
    task: values.task,
    ...(values.base ? { atChangeId: values.base } : {}),
  });
  process.stdout.write(
    `${JSON.stringify({ path: workspace.path, workspaceName: workspace.workspaceName, jjChangeId: workspace.changeId })}\n`,
  );
}

export async function runWarmStartCommand(argv: string[]): Promise<void> {
  const { values } = parseArgs({ args: argv, options: { repo: { type: "string" } }, strict: true });
  if (!values.repo) throw new Error("__warm-start requires --repo");
  ensureJj();
  // A folder Rudder will not colocate (the home folder, a folder of repos)
  // gets no isolated workspaces, so there is nothing to warm.
  if (!isJjRepo(values.repo) && colocateRefusal(values.repo)) return;
  await ensureColocated(values.repo);
  await warmStart(values.repo);
}
