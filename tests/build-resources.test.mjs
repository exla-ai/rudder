import assert from "node:assert/strict";
import test from "node:test";
import { buildJobs, buildResources } from "../cloud/host/build-resources.mjs";

const GiB = 1024 ** 3;
test("builds use the larger home while reserving CPU and memory for agents", () => {
  assert.equal(buildJobs({ cpus: 24, memoryBytes: 92 * GiB }), 22);
  assert.equal(buildJobs({ cpus: 24, memoryBytes: 16 * GiB }), 4);
  assert.equal(buildJobs({ cpus: 1, memoryBytes: GiB }), 1);
});

test("container quotas override a larger visible host", () => {
  const files = { "/sys/fs/cgroup/cpu.max": "400000 100000", "/sys/fs/cgroup/memory.max": String(8 * GiB) };
  const resources = buildResources({ cpus: 24, memoryBytes: 92 * GiB, constrainedMemory: 0, readFile: (file) => files[file] ?? "" });
  assert.deepEqual(resources, { cpus: 4, memoryBytes: 8 * GiB, jobs: 2 });
});

test("unlimited and absent cgroups preserve host capacity; legacy limits still apply", () => {
  const options = { cpus: 24, memoryBytes: 92 * GiB, constrainedMemory: 0 };
  assert.equal(buildResources({ ...options, readFile: () => "max" }).jobs, 22);
  assert.equal(buildResources({ ...options, readFile: () => { throw Error("absent"); } }).jobs, 22);
  const files = { "/sys/fs/cgroup/cpu/cpu.cfs_quota_us": "200000", "/sys/fs/cgroup/cpu/cpu.cfs_period_us": "100000" };
  assert.equal(buildResources({ ...options, readFile: (file) => files[file] ?? "" }).jobs, 1);
  assert.equal(buildResources({ ...options, constrainedMemory: 8 * GiB, readFile: () => "" }).jobs, 2);
});
