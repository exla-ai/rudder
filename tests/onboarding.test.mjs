import assert from "node:assert/strict";
import test from "node:test";

import { chooseReadyBackend } from "../dist/onboarding.js";

test("onboarding points new agents at a signed-in CLI, never an assumed one", () => {
  // A Mac with only Codex signed in, carrying the old hard-coded Claude default.
  const stale = { version: 1, defaultBackend: "claude", lastUsedBackend: "claude", backends: {} };
  assert.equal(chooseReadyBackend(stale, ["codex"]), "codex");
  assert.equal(stale.defaultBackend, "codex");
  assert.equal(stale.lastUsedBackend, undefined);

  // Both ready: the user's pick stays; a fresh config takes the first listed.
  const picked = { version: 1, lastUsedBackend: "codex", backends: {} };
  assert.equal(chooseReadyBackend(picked, ["claude", "codex"]), "codex");
  assert.equal(picked.defaultBackend, undefined);
  const fresh = { version: 1, backends: {} };
  assert.equal(chooseReadyBackend(fresh, ["claude", "codex"]), "claude");

  // Nothing ready leaves nothing chosen; a backend onboarding does not check stays.
  const empty = { version: 1, backends: {} };
  assert.equal(chooseReadyBackend(empty, []), undefined);
  assert.equal(empty.defaultBackend, undefined);
  const opencode = { version: 1, lastUsedBackend: "opencode", backends: {} };
  assert.equal(chooseReadyBackend(opencode, ["codex"]), "opencode");
});
