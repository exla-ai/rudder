import assert from "node:assert/strict";
import test from "node:test";
import { isReadableRunId, newRunId } from "../dist/util.js";
import { workspaceNameFor } from "../dist/jj.js";
import { workspaceDirName } from "../dist/state.js";
import { cloudWorkspaceAbsolutePath } from "../dist/migration.js";

// Mirrors native app_tests::run_ids_are_a_few_task_words_and_a_short_key.
test("run ids are a few task words and a short key", () => {
  const id = newRunId("can you verify the orchard project merged to main");
  const cut = id.lastIndexOf("-");
  assert.equal(id.slice(0, cut), "verify-orchard-project-merged", id);
  assert.match(id.slice(cut + 1), /^[a-z0-9]{5}$/, id);
  assert.ok(isReadableRunId(id), id);
  assert.notEqual(newRunId("shell"), newRunId("shell"));
  assert.ok(newRunId("x".repeat(200)).length <= 38);
  assert.ok(!newRunId("use OPENAI_API_KEY=sk-proj-abcdefghijklmnopqrstuvwx now").includes("abcdefghij"));
});

test("a readable run id names its workspace and folder; old ids keep the old names", () => {
  assert.equal(workspaceNameFor("launch-video-k3f9q"), "rudder-launch-video-k3f9q");
  assert.equal(workspaceDirName("launch-video-k3f9q", "Launch the video"), "launch-video-k3f9q");
  assert.equal(
    cloudWorkspaceAbsolutePath("repo", "launch-video-k3f9q", "Launch the video"),
    "/workspace/.rudder-workspaces/repo/launch-video-k3f9q",
  );

  // A record without a stored name must still derive the name its workspace has.
  const old = "1793065526747296000-branch-verify-orchard-project-merged-to-main-16388";
  assert.ok(!isReadableRunId(old));
  assert.match(workspaceNameFor(old), /^rudder-17930655267472-[0-9a-f]{6}$/);
  assert.match(workspaceDirName(old, "launch video"), /^launch-video-[0-9a-f]{8}$/);
  assert.ok(!isReadableRunId("20261028080035-fix-the-thing-1a2b3c4d"));
  assert.match(workspaceNameFor("n0"), /^rudder-n0-[0-9a-f]{6}$/);
});
