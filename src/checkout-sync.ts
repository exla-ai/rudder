// Bring one checkout up to date with GitHub, which is the source of truth
// between the laptop and the home machine: fetch, then move the checkout to the
// newest default branch only when that cannot lose or tangle work (nothing
// uncommitted, nothing unpushed). Anything else is reported, never forced.
//
// `rudder cloud sync` runs this here and asks the home machine to run the same
// code (`rudder __sync-checkout --json`) in its clone of the repo.

import fs from "node:fs";
import path from "node:path";
import { runCommand } from "./util.js";

export interface CheckoutSyncReport {
  root: string;
  vcs: "jj" | "git";
  branch: string;
  fetched: boolean;
  fetchError?: string;
  // Commits the default branch gained that the checkout does not have.
  behind: number;
  // Local commits not on the remote default branch (unpushed work).
  ahead: number;
  dirty: boolean;
  moved: boolean;
  note: string;
}

async function run(cwd: string, command: string, args: string[]) {
  return await runCommand(command, args, { cwd, allowFailure: true });
}

async function defaultBranch(root: string): Promise<string> {
  const head = await run(root, "git", ["symbolic-ref", "--short", "refs/remotes/origin/HEAD"]);
  const name = head.code === 0 ? head.stdout.trim().replace(/^origin\//, "") : "";
  if (name) {
    return name;
  }
  for (const candidate of ["main", "master"]) {
    if ((await run(root, "git", ["rev-parse", "--verify", "--quiet", `refs/remotes/origin/${candidate}`])).code === 0) {
      return candidate;
    }
  }
  return "main";
}

function count(text: string): number {
  return text.split("\n").filter((line) => line.trim()).length;
}

export async function syncCheckout(root: string): Promise<CheckoutSyncReport> {
  const vcs = fs.existsSync(path.join(root, ".jj")) ? "jj" : "git";
  let fetch = vcs === "jj"
    ? await run(root, "jj", ["git", "fetch"])
    : await run(root, "git", ["fetch", "--prune", "origin"]);
  if (vcs === "jj" && fetch.code !== 0) {
    // jj's fetch needs git >= 2.41 (the cloud image's Debian git is older):
    // fetch with git, then let jj import the moved refs.
    fetch = await run(root, "git", ["fetch", "--prune", "origin"]);
    if (fetch.code === 0) {
      await run(root, "jj", ["git", "import"]);
    }
  }
  const fetched = fetch.code === 0;
  const fetchError = fetched ? undefined : (fetch.stderr || fetch.stdout).trim().split("\n").pop();
  const branch = await defaultBranch(root);
  const report: CheckoutSyncReport = { root, vcs, branch, fetched, fetchError, behind: 0, ahead: 0, dirty: false, moved: false, note: "" };
  return vcs === "jj" ? await syncJj(root, report) : await syncGit(root, report);
}

async function syncGit(root: string, report: CheckoutSyncReport): Promise<CheckoutSyncReport> {
  const remote = `origin/${report.branch}`;
  const current = (await run(root, "git", ["rev-parse", "--abbrev-ref", "HEAD"])).stdout.trim();
  report.dirty = (await run(root, "git", ["status", "--porcelain"])).stdout.trim().length > 0;
  const counts = await run(root, "git", ["rev-list", "--left-right", "--count", `HEAD...${remote}`]);
  const [ahead, behind] = counts.stdout.trim().split(/\s+/).map((value) => Number(value) || 0);
  report.ahead = ahead ?? 0;
  report.behind = behind ?? 0;
  if (report.behind === 0) {
    const parts = [report.ahead > 0 ? `${report.ahead} unpushed commit(s) on ${current}` : "", report.dirty ? "uncommitted edits" : ""].filter(Boolean);
    report.note = parts.length > 0 ? `up to date with ${report.branch}; ${parts.join(", ")}` : "up to date";
    return report;
  }
  if (current !== report.branch) {
    report.note = `on ${current}, not ${report.branch}; left alone`;
  } else if (report.dirty) {
    report.note = `${report.behind} new commit(s) on ${report.branch}, but there are uncommitted changes; not pulled`;
  } else if (report.ahead > 0) {
    report.note = `${report.branch} and this checkout have diverged (${report.ahead} local, ${report.behind} remote); not pulled`;
  } else {
    const merge = await run(root, "git", ["merge", "--ff-only", remote]);
    report.moved = merge.code === 0;
    report.note = report.moved ? `pulled ${report.behind} new commit(s)` : `pull failed: ${(merge.stderr || merge.stdout).trim().split("\n").pop()}`;
  }
  return report;
}

async function syncJj(root: string, report: CheckoutSyncReport): Promise<CheckoutSyncReport> {
  const trunk = `${report.branch}@origin`;
  const jj = async (args: string[]) => await run(root, "jj", ["--ignore-working-copy", ...args]);
  // Snapshot the working copy once so "empty" below is the truth.
  await run(root, "jj", ["status"]);
  const empty = (await jj(["log", "-r", "@", "--no-graph", "-T", 'if(empty, "1", "0")'])).stdout.trim() === "1";
  report.dirty = !empty;
  report.behind = count((await jj(["log", "-r", `::${trunk} ~ ::@`, "--no-graph", "-T", 'commit_id ++ "\\n"'])).stdout);
  // Unpushed: committed changes below @ that the remote branch does not have
  // (@ itself is the uncommitted working copy, reported as dirty).
  report.ahead = count((await jj(["log", "-r", `(::@- ~ ::${trunk}) ~ empty()`, "--no-graph", "-T", 'commit_id ++ "\\n"'])).stdout);
  if (report.behind === 0) {
    const parts = [report.ahead > 0 ? `${report.ahead} unpushed change(s)` : "", report.dirty ? "uncommitted edits in the working copy" : ""].filter(Boolean);
    report.note = parts.length > 0 ? `up to date with ${report.branch}; ${parts.join(", ")}` : "up to date";
    return report;
  }
  if (!empty) {
    report.note = `${report.behind} new commit(s) on ${report.branch}, but the working copy has uncommitted edits; not moved (\`jj rebase -d ${trunk}\` when ready)`;
  } else if (report.ahead > 0) {
    report.note = `${report.behind} new commit(s) on ${report.branch}, and ${report.ahead} unpushed change(s) here; not moved`;
  } else {
    // An empty, undescribed @ is abandoned by jj when the working copy moves.
    const moved = await run(root, "jj", ["new", trunk]);
    report.moved = moved.code === 0;
    report.note = report.moved ? `moved to the newest ${report.branch} (${report.behind} new commit(s))` : `move failed: ${(moved.stderr || moved.stdout).trim().split("\n").pop()}`;
  }
  return report;
}

export function describeSync(where: string, report: CheckoutSyncReport): string {
  const fetch = report.fetched ? "" : ` (fetch failed: ${report.fetchError ?? "unknown error"})`;
  return `${where}: ${report.note}${fetch}`;
}
