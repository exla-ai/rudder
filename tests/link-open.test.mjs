import test from "node:test";
import assert from "node:assert/strict";
import { LinkOpener } from "../dist/link-open.js";

const URL = "https://github.com/openai/codex/blob/main/codex-rs/tui/src/history_cell.rs?plain=1&q=abc";
const request = (url, end = "\x07") => Buffer.from(`\x1b]7770;open;${url}${end}`, "utf8");
const click = Buffer.from("\x1b[<0;12;5M\x1b[<0;12;5m");

function opener() {
  let clock = 50_000;
  const opened = [];
  const links = new LinkOpener((url) => opened.push(url), () => clock);
  return { links, opened, tick: (ms) => { clock += ms; } };
}

test("a link the dashboard asks for right after a click opens here, once", () => {
  const { links, opened, tick } = opener();
  links.noteInput(click);
  tick(400);
  const frame = Buffer.concat([Buffer.from("\x1b[3;1Hhello"), request(URL), Buffer.from(" opened")]);
  assert.deepEqual(links.noteOutput(frame), [URL]);
  assert.deepEqual(opened, [URL]);
  // The same request again (a replay, a repaint) without a new click: nothing.
  assert.deepEqual(links.noteOutput(request(URL)), []);
  assert.deepEqual(opened, [URL]);
});

test("replayed output with no click behind it opens nothing", () => {
  const { links, opened, tick } = opener();
  assert.deepEqual(links.noteOutput(request(URL)), []);
  // A click long ago does not count either.
  links.noteInput(click);
  tick(6_000);
  assert.deepEqual(links.noteOutput(request(URL)), []);
  assert.deepEqual(opened, []);
});

test("only left clicks count, not the wheel, a drag or a press alone", () => {
  const { links, opened } = opener();
  links.noteInput(Buffer.from("\x1b[<64;12;5M\x1b[<65;12;5M")); // wheel
  links.noteInput(Buffer.from("\x1b[<32;14;5M")); // drag motion
  links.noteInput(Buffer.from("\x1b[<2;12;5m")); // right button release
  links.noteInput(Buffer.from("\x1b[<0;12;5M")); // left press, no release yet
  assert.deepEqual(links.noteOutput(request(URL)), []);
  links.noteInput(Buffer.from("\x1b[<4;12;5m")); // shift + left release
  assert.deepEqual(links.noteOutput(request(URL)), [URL]);
  assert.deepEqual(opened, [URL]);
});

test("a request split across frames still opens", () => {
  const { links, opened } = opener();
  links.noteInput(click);
  const bytes = Buffer.concat([Buffer.from("frame "), request(URL, "\x1b\\")]);
  for (const cut of [7, 9, 14, 30, bytes.length - 1]) {
    const fresh = opener();
    fresh.links.noteInput(click);
    assert.deepEqual(fresh.links.noteOutput(bytes.subarray(0, cut)), [], `cut at ${cut}`);
    assert.deepEqual(fresh.links.noteOutput(bytes.subarray(cut)), [URL], `cut at ${cut}`);
  }
  assert.deepEqual(links.noteOutput(bytes), [URL]);
  assert.deepEqual(opened, [URL]);
});

test("only web and mail links open; anything else stays inert", () => {
  for (const url of ["file:///etc/passwd", "javascript:alert(1)", "vscode://x", "https://a b", "ssh://h"]) {
    const { links, opened } = opener();
    links.noteInput(click);
    assert.deepEqual(links.noteOutput(request(url)), [], url);
    assert.deepEqual(opened, [], url);
  }
  const { links } = opener();
  links.noteInput(click);
  assert.deepEqual(links.noteOutput(request("mailto:a@b.dev")), ["mailto:a@b.dev"]);
  links.noteInput(click);
  assert.deepEqual(links.noteOutput(request("https://例え.jp/パス")), ["https://例え.jp/パス"]);
});
