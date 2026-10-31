import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import fs from "node:fs/promises";
import path from "node:path";

// Locate only this disposable project's endpoint. Long macOS temporary homes
// use the shared private directory; never enumerate or remove its other files.
export async function boardHostFiles(repo, home) {
  const key = createHash("sha256").update(await fs.realpath(repo)).digest("hex").slice(0, 32);
  const candidates = [path.join(home, "board", `${key}.json`), `/tmp/rudder-board-${process.getuid()}/${key}.json`];
  const found = [];
  for (const file of candidates) {
    try { await fs.lstat(file); found.push(file); }
    catch (error) { if (error.code !== "ENOENT") throw error; }
  }
  assert.equal(found.length, 1, "this project has exactly one private board descriptor");
  return { descriptor: found[0], socket: found[0].replace(/\.json$/, ".sock") };
}
