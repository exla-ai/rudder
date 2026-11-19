// How the app's Cmd+K palette orders its rows for what is typed
// (cloud/app/command-rank.js).
import test from "node:test";
import assert from "node:assert/strict";
import { rankCommands } from "../cloud/app/command-rank.js";

const rows = [
  { label: "Open terminal", keywords: ["shell", "console"] },
  { label: "New chat", keywords: ["agent", "task"] },
  { label: "Notes" },
  { label: "Files" },
  { label: "Settings" },
  { label: "Phone app for agent control", keywords: ["rudder"] },
  { label: "Settle bets on resolution", keywords: ["wagerpals"] },
  { label: "Port scanner for the home", keywords: ["rudder"] },
  { label: "Bump jj to 0.40 flags", keywords: ["rudder"], searchOnly: true },
  { label: "rudder" },
  { label: "wagerpals" },
  { label: "New chat in wagerpals", keywords: ["agent", "task"], searchOnly: true },
];
const labels = (q) => rankCommands(rows, q).map((r) => r.label);

test("nothing typed keeps the order and leaves out the rows shown only for a search", () => {
  assert.deepEqual(labels(""), labels("   "));
  assert.equal(labels("").length, rows.length - 2);
  assert.ok(!labels("").includes("Bump jj to 0.40 flags"));
  assert.ok(!labels("").includes("New chat in wagerpals"));
});

test("a whole label, then its start, then a word in it", () => {
  assert.deepEqual(labels("rudder").slice(0, 1), ["rudder"]);
  assert.deepEqual(labels("se"), ["Settings", "Settle bets on resolution"]);
  assert.deepEqual(labels("new"), ["New chat", "New chat in wagerpals"]);
  assert.deepEqual(labels("new agent"), ["New chat", "New chat in wagerpals"], "agent is a keyword");
  assert.deepEqual(labels("OPEN T"), ["Open terminal"]);
});

test("a project's name finds its agents after the project, older ones included", () => {
  assert.deepEqual(labels("rudder"), ["rudder", "Phone app for agent control", "Port scanner for the home", "Bump jj to 0.40 flags"]);
  assert.deepEqual(labels("wager"), ["wagerpals", "New chat in wagerpals", "Settle bets on resolution"]);
  assert.deepEqual(labels("phone rud"), ["Phone app for agent control"]);
  assert.deepEqual(labels("shell"), ["Open terminal"]);
});

test("text inside a word matches last; scattered letters do not", () => {
  assert.deepEqual(labels("ttings"), ["Settings"]);
  assert.deepEqual(labels("term"), ["Open terminal"], "not p-o-r-T scann-E-R ho-M-e");
  assert.deepEqual(labels("optrm"), []);
  assert.deepEqual(labels("zzz"), []);
});
