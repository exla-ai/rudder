import test from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { PasteBridge, pastedLocalFiles, isCtrlV } from "../dist/paste-bridge.js";

const PB = "\x1b[200~";
const PE = "\x1b[201~";

function fixture() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-paste-"));
  const shot = path.join(dir, "Screen Shot 1.png");
  fs.writeFileSync(shot, "png");
  const notes = path.join(dir, "notes.txt");
  fs.writeFileSync(notes, "hi");
  return { dir, shot, notes };
}

function bridge(extra = {}) {
  const sent = [];
  const uploads = [];
  const b = new PasteBridge({
    send: (data) => sent.push(data.toString("utf8")),
    upload: async (file, name) => {
      uploads.push(name);
      await new Promise((resolve) => setTimeout(resolve, 20));
      return `/workspace/code/repo/.rudder/uploads/x-${name.replace(/ /g, "-")}`;
    },
    ...extra,
  });
  const idle = () => new Promise((resolve) => setTimeout(resolve, 80));
  return { b, sent, uploads, idle };
}

test("only pastes made entirely of existing local files count", () => {
  const { dir, shot, notes } = fixture();
  assert.deepEqual(pastedLocalFiles(`'${shot}' `), [shot]);
  assert.deepEqual(pastedLocalFiles(shot.replace(/ /g, "\\ ")), [shot]);
  assert.deepEqual(pastedLocalFiles(`"${shot}" ${notes}`), [shot, notes]);
  assert.deepEqual(pastedLocalFiles(`file://${encodeURI(shot)}`), [shot]);
  assert.equal(pastedLocalFiles(`see ${notes}`), null);
  assert.equal(pastedLocalFiles(dir), null, "a directory is not a file");
  assert.equal(pastedLocalFiles(path.join(dir, "gone.png")), null);
  assert.equal(pastedLocalFiles("const x = 1;"), null);
});

test("a dropped file goes up first and the cloud path is pasted, keys after it wait their turn", async () => {
  const { shot } = fixture();
  const { b, sent, uploads, idle } = bridge();
  b.forward(Buffer.from("a"));
  b.forward(Buffer.from(`${PB}'${shot}' ${PE}`));
  b.forward(Buffer.from("b"));
  await idle();
  assert.deepEqual(uploads, ["Screen Shot 1.png"]);
  assert.deepEqual(sent, ["a", `${PB}/workspace/code/repo/.rudder/uploads/x-Screen-Shot-1.png ${PE}`, "b"]);
});

test("ordinary pastes and keys pass straight through", async () => {
  const { b, sent, uploads, idle } = bridge();
  b.forward(Buffer.from(`${PB}hello /etc and more${PE}`));
  b.forward(Buffer.from("/"));
  await idle();
  assert.deepEqual(uploads, []);
  assert.deepEqual(sent, [`${PB}hello /etc and more${PE}`, "/"]);
});

test("Ctrl+V with an image on the clipboard pastes the uploaded image; without one it is just Ctrl+V", async () => {
  const { dir } = fixture();
  const clip = path.join(dir, "clip.png");
  let contents = null;
  const { b, sent, uploads, idle } = bridge({ readClipboard: async () => contents });
  assert.ok(isCtrlV(Buffer.from("\x16")) && isCtrlV(Buffer.from("\x1b[118;5u")));
  b.forward(Buffer.from("\x16"));
  await idle();
  assert.deepEqual(sent, ["\x16"]);
  fs.writeFileSync(clip, "png");
  contents = { image: clip };
  b.forward(Buffer.from("\x1b[118;5u"));
  await idle();
  assert.deepEqual(uploads, ["clipboard.png"]);
  assert.equal(sent[1], `${PB}/workspace/code/repo/.rudder/uploads/x-clipboard.png${PE}`);
  assert.ok(!fs.existsSync(clip), "the temporary clipboard file is removed");
});

test("a failed upload still delivers what was typed and says why", async () => {
  const { shot } = fixture();
  const statuses = [];
  const sent = [];
  const b = new PasteBridge({
    send: (data) => sent.push(data.toString("utf8")),
    upload: async () => { throw new Error("that file is over 25 MB"); },
    status: (text) => statuses.push(text),
  });
  b.forward(Buffer.from(`${PB}'${shot}'${PE}`));
  await new Promise((resolve) => setTimeout(resolve, 30));
  assert.deepEqual(sent, [`${PB}'${shot}'${PE}`]);
  assert.ok(statuses.some((text) => text && /over 25 MB/.test(text)));
});
