// Bring one checkout up to date with GitHub, which is the source of truth
// between the laptop and the home machine: fetch, then move the checkout to the
// newest default branch only when that cannot lose or tangle work (nothing
// uncommitted, nothing unpushed). Anything else is reported, never forced.
//
// `rudder cloud sync` runs this here and asks the home machine to run the same
// code (`rudder __sync-checkout`) in its clone of the repo.
//
// Work GitHub does not have yet (the jj working copy and unpushed changes below
// it) travels between the two machines directly, as a git bundle through the
// control plane, never through GitHub: jj tracks every file that is not
// gitignored, so a pushed work-in-progress branch could carry a stray secret.
// The receiving side moves onto the incoming work only when that loses
// nothing (it has no work of its own, or its work is already in the incoming
// history); otherwise both stay, the incoming one at bookmark rudder-sync/<from>.

import fs from "node:fs";
import path from "node:path";
import { runCommand } from "./util.js";
import { colocateRefusal, ensureJj } from "./jj.js";

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
  work?: WorkReport;
}

export interface WorkReport {
  // This side's work, bundled for the other side.
  sent?: { head: string; commits: number };
  // The other side's work, as received here.
  received?: { from: string; head: string; outcome: "moved" | "have" | "diverged" | "failed"; note: string };
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
  const lines = [`${where}: ${report.note}${fetch}`];
  if (report.work?.received) {
    lines.push(`  received: ${report.work.received.note}`);
  }
  if (report.work?.sent) {
    lines.push(`  sent: ${report.work.sent.commits} change(s) not on GitHub yet`);
  }
  return lines.join("\n");
}

const OUT_REF = "refs/rudder-sync/out";
const SIDE_RE = /^[a-z][a-z0-9-]{0,30}$/;

async function trunkCommit(root: string): Promise<{ branch: string; sha: string | null }> {
  const branch = await defaultBranch(root);
  const sha = await run(root, "git", ["rev-parse", "--verify", "--quiet", `refs/remotes/origin/${branch}`]);
  return { branch, sha: sha.code === 0 ? sha.stdout.trim() : null };
}

async function jjOne(root: string, revset: string): Promise<string> {
  const out = await run(root, "jj", ["--ignore-working-copy", "log", "-r", revset, "--no-graph", "-T", 'commit_id ++ "\\n"']);
  return out.code === 0 ? out.stdout.trim().split("\n")[0]?.trim() ?? "" : "";
}

// Does this side hold work GitHub lacks? Returns its head: the working copy when
// it has edits, else the newest unpushed change below it.
async function workHead(root: string, trunk: string): Promise<{ head: string; empty: boolean } | null> {
  await run(root, "jj", ["status"]);
  const empty = (await run(root, "jj", ["--ignore-working-copy", "log", "-r", "@", "--no-graph", "-T", 'if(empty, "1", "0")'])).stdout.trim() === "1";
  if (!empty) {
    return { head: await jjOne(root, "@"), empty };
  }
  const unpushed = await jjOne(root, `(::@- ~ ::${trunk}) ~ empty()`);
  return unpushed ? { head: await jjOne(root, "@-"), empty } : null;
}

/**
 * Bundle this side's unpushed work into `outFile`. A working copy with edits
 * is checkpointed first (`jj new`), so later edits here land in a CHILD of the
 * sent change and the other side can still fast-forward to them next time.
 */
export async function exportWork(root: string, outFile: string): Promise<WorkReport["sent"] | null> {
  if (!fs.existsSync(path.join(root, ".jj"))) {
    return null;
  }
  const { branch, sha } = await trunkCommit(root);
  const work = await workHead(root, `${branch}@origin`);
  if (!work?.head) {
    return null;
  }
  if (!work.empty) {
    await run(root, "jj", ["new"]);
  }
  await run(root, "git", ["update-ref", OUT_REF, work.head]);
  try {
    const args = ["bundle", "create", outFile, OUT_REF, ...(sha ? ["--not", sha] : [])];
    const bundled = await run(root, "git", args);
    if (bundled.code !== 0) {
      throw new Error(`git bundle failed: ${(bundled.stderr || bundled.stdout).trim().split("\n").pop()}`);
    }
  } finally {
    await run(root, "git", ["update-ref", "-d", OUT_REF]);
  }
  const counted = await run(root, "git", ["rev-list", "--count", work.head, ...(sha ? ["--not", sha] : [])]);
  return { head: work.head, commits: Number(counted.stdout.trim()) || 0 };
}

// Every file the working copy changed reads the same in `head`.
async function editsContainedIn(root: string, head: string): Promise<boolean> {
  const listed = await run(root, "jj", ["--ignore-working-copy", "diff", "-r", "@", "--name-only"]);
  const files = listed.stdout.split("\n").map((line) => line.trim()).filter(Boolean);
  if (listed.code !== 0 || files.length > 200) {
    return false;
  }
  for (const file of files) {
    const fileset = `root-file:${JSON.stringify(file)}`;
    const here = await run(root, "jj", ["--ignore-working-copy", "file", "show", "-r", "@", fileset]);
    const there = await run(root, "jj", ["--ignore-working-copy", "file", "show", "-r", head, fileset]);
    if ((here.code === 0) !== (there.code === 0) || here.stdout !== there.stdout) {
      return false;
    }
  }
  return true;
}

/** Take the other side's work from a bundle made by exportWork. */
export async function importWork(root: string, bundle: string, head: string, from: string): Promise<NonNullable<WorkReport["received"]>> {
  const report = (outcome: NonNullable<WorkReport["received"]>["outcome"], note: string) => ({ from, head, outcome, note });
  if (!SIDE_RE.test(from) || !/^[0-9a-f]{40}$/.test(head)) {
    return report("failed", "bad sync request");
  }
  if (!fs.existsSync(path.join(root, ".jj"))) {
    // Plain colocate: Rudder's ignore entries would be an edit of this side's own.
    ensureJj();
    const refusal = colocateRefusal(root);
    if (refusal) return report("failed", refusal);
    await run(root, "jj", ["git", "init", "--colocate"]);
  }
  const bookmark = `rudder-sync/${from}`;
  const fetched = await run(root, "git", ["fetch", "-q", bundle, `+${OUT_REF}:refs/heads/${bookmark}`]);
  if (fetched.code !== 0) {
    return report("failed", `could not read the work from ${from}: ${(fetched.stderr || fetched.stdout).trim().split("\n").pop()}`);
  }
  await run(root, "jj", ["git", "import"]);
  const { branch } = await trunkCommit(root);
  const trunk = `${branch}@origin`;
  const drop = async () => {
    await run(root, "jj", ["bookmark", "delete", bookmark]);
  };
  if (await jjOne(root, `${head} & ::${trunk}`)) {
    await drop();
    return report("have", "already on GitHub");
  }
  const mine = await workHead(root, trunk);
  if (await jjOne(root, `${head} & ::@`)) {
    await drop();
    return report("have", "already here");
  }
  // The working copy's own edits count as work only when the incoming history
  // lacks them (Rudder writing its .gitignore entries into a fresh clone does not).
  const at = await jjOne(root, "@");
  const parentHeld = !(await jjOne(root, `(::@- ~ ::${trunk}) ~ empty()`)) || !!(await jjOne(root, `@- & ::${head}`));
  const subsumed = !!mine && !mine.empty && parentHeld && (await editsContainedIn(root, head));
  if (!mine || subsumed || (await jjOne(root, `${mine.head} & ::${head}`))) {
    const moved = await run(root, "jj", ["new", head]);
    if (moved.code !== 0) {
      return report("failed", `could not move onto it: ${(moved.stderr || moved.stdout).trim().split("\n").pop()}`);
    }
    if (subsumed) {
      await run(root, "jj", ["abandon", at]);
    }
    await drop();
    return report("moved", `picked up the work from ${from}`);
  }
  return report("diverged", `both sides have their own edits; kept both. ${from === "cloud" ? "The cloud's" : `The ${from}'s`} version is bookmark ${bookmark} (\`jj new ${bookmark}\` to switch to it, \`jj diff --from @ --to ${bookmark}\` to compare)`);
}
