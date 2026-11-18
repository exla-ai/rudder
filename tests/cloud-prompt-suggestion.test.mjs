import test from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { promptSuggestion, suggestionFile } from "../cloud/worker/suggestion.mjs";

function withHome(fn) {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-suggest-"));
  try {
    fn(home);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
}

function save(home, runId, body) {
  const file = suggestionFile(home, runId);
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, body);
}

test("a shown suggestion reads back; a cleared, torn or missing one is none", () => {
  withHome((home) => {
    assert.equal(promptSuggestion(home, "run_a"), null, "no file: the backend never suggested");

    save(home, "run_a", JSON.stringify({ text: "  run the tests you just wrote\n", at: 1763452800000 }));
    assert.deepEqual(promptSuggestion(home, "run_a"), { text: "run the tests you just wrote", at: 1763452800000 });

    save(home, "run_a", JSON.stringify({ text: null, at: 1763452860000 }));
    assert.equal(promptSuggestion(home, "run_a"), null, "a turn started");

    save(home, "run_a", '{"text":"run the te');
    assert.equal(promptSuggestion(home, "run_a"), null, "read mid-write");

    save(home, "run_a", JSON.stringify({ text: "commit it" }));
    assert.throws(() => promptSuggestion(home, "run_a"), /without its time/);
  });
});
