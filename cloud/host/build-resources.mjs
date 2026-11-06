// Local build wrappers use the current machine, rather than the Fly home's size.
import fs from "node:fs";
import os from "node:os";
import { pathToFileURL } from "node:url";

export function buildJobs({ cpus, memoryBytes }) {
  const cpuCount = Math.max(1, Math.floor(cpus));
  const cpuBudget = Math.max(1, cpuCount - (cpuCount >= 8 ? 2 : 1));
  // Leave room for agents and the supervisor; large Rust crates use several GiB.
  const memoryBudget = Math.max(1, Math.floor((memoryBytes - 2 * 1024 ** 3) / (3 * 1024 ** 3)));
  return Math.min(cpuBudget, memoryBudget);
}

export function buildResources({
  cpus = os.availableParallelism(),
  memoryBytes = os.totalmem(),
  constrainedMemory = process.constrainedMemory?.() ?? 0,
  readFile = (file) => fs.readFileSync(file, "utf8"),
} = {}) {
  const read = (file) => { try { return readFile(file).trim(); } catch { return ""; } };
  // availableParallelism observes affinity, but CPU bandwidth quotas can be lower.
  const [quota, period] = read("/sys/fs/cgroup/cpu.max").split(/\s+/).map(Number);
  const legacyQuota = Number(read("/sys/fs/cgroup/cpu/cpu.cfs_quota_us"));
  const legacyPeriod = Number(read("/sys/fs/cgroup/cpu/cpu.cfs_period_us"));
  for (const [q, p] of [[quota, period], [legacyQuota, legacyPeriod]]) {
    if (q > 0 && p > 0) cpus = Math.min(cpus, Math.max(1, Math.floor(q / p)));
  }
  for (const limit of [constrainedMemory, Number(read("/sys/fs/cgroup/memory.max")), Number(read("/sys/fs/cgroup/memory/memory.limit_in_bytes"))]) {
    if (limit > 0) memoryBytes = Math.min(memoryBytes, limit);
  }
  return { cpus, memoryBytes, jobs: buildJobs({ cpus, memoryBytes }) };
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const resources = buildResources();
  console.log(process.argv.includes("--jobs") ? resources.jobs : JSON.stringify(resources));
}
