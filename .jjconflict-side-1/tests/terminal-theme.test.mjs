// The laptop terminal's colors reach the cloud machine (src/terminal-theme.ts,
// cloud/worker/theme.mjs), so agents there theme themselves light or dark.
import test from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { TerminalThemeWatcher } from "../dist/terminal-theme.js";
import { writeTerminalColors } from "../cloud/worker/theme.mjs";

test("color answers are taken out of the keys and reported", () => {
  const seen = [];
  let asked = 0;
  const watcher = new TerminalThemeWatcher(() => { asked += 1; }, (colors) => seen.push(colors));
  const keys = watcher.filter(Buffer.from("a\x1b]10;rgb:1c1c/1c1c/1c1c\x1b\\\x1b]11;rgb:ffff/ffff/fefe\x07b"));
  assert.equal(keys.toString(), "ab");
  assert.deepEqual(seen, [{ fg: "#1c1c1c", bg: "#fffffe" }]);
  assert.equal(watcher.known, true);
  // The same answer again is not news.
  watcher.filter(Buffer.from("\x1b]11;rgb:ffff/ffff/fefe\x07"));
  assert.equal(seen.length, 1);
  // An appearance switch asks again and is not a key either.
  assert.equal(watcher.filter(Buffer.from("\x1b[?997;1n")).length, 0);
  assert.equal(asked, 1);
  // Plain keys pass through untouched (same buffer).
  const plain = Buffer.from("hello");
  assert.equal(watcher.filter(plain), plain);
});

test("the supervisor saves well-formed colors once", () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-theme-"));
  const file = path.join(dir, "terminal-colors.json");
  assert.equal(writeTerminalColors({ type: "theme", fg: "#111111", bg: "#FAFAFA" }, file), true);
  assert.deepEqual(JSON.parse(fs.readFileSync(file, "utf8")), { fg: "#111111", bg: "#fafafa" });
  assert.equal(writeTerminalColors({ type: "theme", fg: "#111111", bg: "#fafafa" }, file), false);
  assert.equal(writeTerminalColors({ type: "theme", bg: "red; rm -rf" }, file), false);
  fs.rmSync(dir, { recursive: true, force: true });
});
