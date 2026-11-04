// The terminal QR encoder `rudder login` prints the approve link with. Checked
// against libqrencode when it is installed: both matrices are unmasked with the
// mask their own format bits name, and must then agree module for module (the
// two encoders may pick different masks, which is legal).
import { test } from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";

import { encodeQr, maskBit, renderQrForTerminal } from "../dist/qr.js";

const hasQrencode = spawnSync("qrencode", ["--version"]).status === 0;

function qrencodeMatrix(text, level) {
  const out = spawnSync("qrencode", ["-8", "-l", level, "-m", "0", "-t", "ASCII", "-o", "-", text], { encoding: "utf8" });
  assert.equal(out.status, 0, out.stderr);
  return out.stdout.replace(/\n+$/, "").split("\n").map((line) => {
    const row = [];
    for (let i = 0; i < line.length; i += 2) row.push(line[i] === "#");
    return row;
  });
}

// Format bits next to the top-left finder, unmasked with 0x5412.
function formatOf(m) {
  let bits = 0;
  const cells = [];
  for (let i = 0; i <= 5; i += 1) cells.push([8, i]);
  cells.push([8, 7], [8, 8], [7, 8]);
  for (let i = 9; i < 15; i += 1) cells.push([14 - i, 8]);
  cells.forEach(([x, y], i) => { if (m[y][x]) bits |= 1 << i; });
  bits ^= 0x5412;
  return { level: bits >>> 13, mask: (bits >>> 10) & 7 };
}

// Function modules carry no mask: finders + format strips, timing, alignment
// patterns and (version 7+) the version blocks.
function isFormatOrFixed(size, x, y) {
  const inFinder = (x < 9 && y < 9) || (x >= size - 8 && y < 9) || (x < 9 && y >= size - 8);
  if (inFinder || x === 6 || y === 6) return true;
  const version = (size - 17) / 4;
  if (version >= 7 && ((x >= size - 11 && y < 6) || (y >= size - 11 && x < 6))) return true;
  if (version < 2) return false;
  const count = Math.floor(version / 7) + 2;
  const step = Math.ceil((version * 4 + 4) / (count * 2 - 2)) * 2;
  const positions = [6];
  for (let pos = size - 7; positions.length < count; pos -= step) positions.splice(1, 0, pos);
  const last = positions.length - 1;
  return positions.some((ax, i) => positions.some((ay, j) => {
    const corner = (i === 0 && j === 0) || (i === 0 && j === last) || (i === last && j === 0);
    return !corner && Math.abs(x - ax) <= 2 && Math.abs(y - ay) <= 2;
  }));
}

function unmasked(m) {
  const { mask } = formatOf(m);
  const size = m.length;
  return m.map((row, y) => row.map((dark, x) => (isFormatOrFixed(size, x, y) ? dark : dark !== maskBit(mask, x, y))));
}

for (const text of [
  "hi",
  "https://rudder-cloud-control.fly.dev/app/approve?code=K7P-4QX",
  "http://localhost:3000/app/approve?code=ABC-DEF",
  "x".repeat(150),
]) {
  for (const level of ["L", "M"]) {
    test(`QR for ${text.length} bytes at level ${level} matches libqrencode`, { skip: !hasQrencode && "qrencode not installed" }, () => {
      const mine = encodeQr(text, level);
      const theirs = qrencodeMatrix(text, level);
      assert.equal(mine.length, theirs.length, "same version");
      assert.equal(formatOf(mine).level, level === "L" ? 1 : 0);
      // Alignment patterns sit outside the excluded strips and are mask-free,
      // so a straight compare after unmasking covers them too.
      const a = unmasked(mine);
      const b = unmasked(theirs);
      const size = a.length;
      for (let y = 0; y < size; y += 1) {
        for (let x = 0; x < size; x += 1) {
          if (!isFormatOrFixed(size, x, y)) assert.equal(a[y][x], b[y][x], `module ${x},${y}`);
          // Function patterns are never masked; only the format strips may
          // differ (they name each encoder's mask).
          else if (x !== 8 && y !== 8) assert.equal(mine[y][x], theirs[y][x], `function module ${x},${y}`);
        }
      }
    });
  }
}

test("QR terminal rendering is black on white with a quiet zone", () => {
  const out = renderQrForTerminal("hi", { indent: "  " });
  const lines = out.split("\n");
  // Version 1 is 21 modules, plus 2 on each side, two rows per line.
  assert.equal(lines.length, Math.ceil(25 / 2));
  for (const line of lines) {
    assert.ok(line.startsWith("  \x1b[30;47m"));
    assert.ok(line.endsWith("\x1b[0m"));
  }
  assert.throws(() => encodeQr("x".repeat(400)), /too long/);
});
