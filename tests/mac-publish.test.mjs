import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

const publish = fileURLToPath(new URL("../mac/scripts/publish.sh", import.meta.url));
const runLookup = "api repos/exla-ai/rudder/actions/runs/42?exclude_pull_requests=true";

for (const status of [404, 403]) {
  test(`Mac publisher handles an HTTP ${status} lookup of the dispatched run`, (t) => {
    const dir = mkdtempSync(join(tmpdir(), "rudder-mac-publish-"));
    t.after(() => rmSync(dir, { recursive: true, force: true }));
    writeFileSync(join(dir, "Rudder.dmg"), "fixture");
    writeFileSync(join(dir, "release.env"), "VERSION=5.0.20\nBUILD=5000020\nMINIMUM_SYSTEM=14.0\n");
    writeFileSync(join(dir, "sleep"), "#!/bin/sh\nexit 0\n", { mode: 0o755 });
    writeFileSync(join(dir, "gh"), `#!/bin/bash
printf '%s %s\n' "$1" "$2" >> "$CALL_LOG"
case "$1 $2" in
  'api repos/exla-ai/rudder/commits/candidate') echo candidate ;;
  'release view')
    if [[ "$*" == *isDraft* ]]; then echo 'release not found' >&2; exit 1; fi
    echo 'https://github.com/exla-ai/rudder/releases/tag/mac-v5.0.20-5000020' ;;
  'release create'|'run watch') ;;
  'workflow run') echo 'https://github.com/exla-ai/rudder/actions/runs/42' ;;
  '${runLookup}')
    if [[ ! -e "$LOOKUP_SEEN" ]]; then
      touch "$LOOKUP_SEEN"
      echo "gh: Lookup failed (HTTP $LOOKUP_STATUS)" >&2
      exit 1
    fi ;;
  *) exit 2 ;;
esac
`, { mode: 0o755 });

    const callLog = join(dir, "calls");
    const result = spawnSync("bash", [publish, dir, "candidate"], {
      encoding: "utf8",
      env: {
        ...process.env,
        PATH: `${dir}:${process.env.PATH}`,
        CALL_LOG: callLog,
        LOOKUP_SEEN: join(dir, "first-lookup"),
        LOOKUP_STATUS: String(status),
      },
    });
    const calls = readFileSync(callLog, "utf8").trim().split("\n");
    if (status === 404) {
      assert.equal(result.status, 0, result.stderr);
      assert.deepEqual(calls.slice(-4), [runLookup, runLookup, "run watch", "release view"]);
    } else {
      assert.equal(result.status, 1);
      assert.match(result.stderr, /cannot read run.*HTTP 403/);
      assert.deepEqual(calls.slice(-2), ["workflow run", runLookup]);
    }
  });
}
