import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");

// The shipped namecheap CLI against canned API responses: paging, parsing, and
// that anything which spends money or moves DNS refuses without --yes.
test("the namecheap skill's CLI parses the API and gates spending", { skip: spawnSync("python3", ["--version"]).status !== 0 && "python3 missing" }, () => {
  const result = spawnSync("python3", ["-I", "-B", path.join(root, "tests", "namecheap_cli_check.py"), path.join(root, "assets", "skills", "namecheap", "scripts", "namecheap")], { encoding: "utf8" });
  assert.equal(result.status, 0, result.stderr + result.stdout);
  assert.match(result.stdout, /ALL OK/);
});
