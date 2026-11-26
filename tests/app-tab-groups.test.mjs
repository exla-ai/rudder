import assert from "node:assert/strict";
import test from "node:test";
import { COLORS, arrange, groupColors, groupsOf, ownColor, placeFor, regroup } from "../cloud/app/tab-groups.js";

// A tab is its id; its group is the letter before the digit, "-" for no project.
const tab = (id) => ({ id });
const keyOf = (t) => (t.id[0] === "-" ? null : t.id[0]);
const ids = (tabs) => tabs.map((t) => t.id).join(" ");
const list = (s) => s.split(" ").map(tab);

test("pages of no project come first, then each project's tabs as the projects first appear", () => {
  assert.equal(ids(arrange(list("a1 b1 -1 a2 c1 b2 -2"), keyOf)), "-1 -2 a1 a2 b1 b2 c1");
});

test("a new tab goes to the end of its group, a new project's to the end of the list", () => {
  const tabs = list("-1 a1 a2 b1");
  assert.equal(placeFor(tabs, "a", keyOf), 3);
  assert.equal(placeFor(tabs, "b", keyOf), 4);
  assert.equal(placeFor(tabs, "c", keyOf), 4);
  assert.equal(placeFor(tabs, null, keyOf), 1);
  assert.equal(placeFor(list("a1"), null, keyOf), 0);
});

test("a tab that goes to another project's page joins the end of that group", () => {
  const tabs = list("-1 a1 a2 b1 b2");
  const moved = tabs[1];
  moved.id = "b3";
  assert.equal(ids(regroup(tabs, moved, keyOf)), "-1 a2 b1 b2 b3");
});

test("a tab that becomes a project's first stays near where it was", () => {
  const alone = list("a1 b1 c1");
  alone[1].id = "d1";
  assert.equal(ids(regroup(alone, alone[1], keyOf)), "a1 d1 c1");
  const inside = list("a1 a2 a3 b1");
  inside[1].id = "d1";
  assert.equal(ids(regroup(inside, inside[1], keyOf)), "a1 a3 d1 b1");
});

test("a tab that leaves for a page of no project joins those at the start", () => {
  const tabs = list("-1 a1 a2");
  tabs[2].id = "-2";
  assert.equal(ids(regroup(tabs, tabs[2], keyOf)), "-1 -2 a1");
  const none = list("a1 a2");
  none[1].id = "-1";
  assert.equal(ids(regroup(none, none[1], keyOf)), "-1 a1");
});

test("groups read off in order", () => {
  assert.deepEqual(groupsOf(list("-1 a1 a2 b1"), keyOf).map((g) => [g.key, ids(g.tabs)]), [[null, "-1"], ["a", "a1 a2"], ["b", "b1"]]);
});

test("a project's own color comes from its id, is the same every time, and is never the grey", () => {
  const ids = Array.from({ length: 200 }, (_, i) => `ws-${i.toString(36)}-${(i * 7919).toString(16)}`);
  const own = ids.map(ownColor);
  assert.deepEqual(ids.map(ownColor), own);
  assert.ok(own.every((c) => COLORS.includes(c) && c !== "grey"));
  assert.equal(new Set(own).size, COLORS.length - 1, "every hue gets used");
});

test("open projects take their own colors, and one whose color is taken moves to the next free hue", () => {
  const hues = COLORS.filter((c) => c !== "grey");
  // Ids by their own color: two share rose, one is plum.
  const pool = Array.from({ length: 400 }, (_, i) => `p${i}`);
  const of = (c) => pool.filter((k) => ownColor(k) === c);
  const [a, b] = of("rose").sort();
  const [c] = of("plum");
  const none = () => null;
  const colors = groupColors([b, c, a], none);
  assert.equal(colors.get(a), "rose", "the first by id keeps it, wherever its group sits");
  assert.equal(colors.get(c), "plum", "a project whose color is free keeps it");
  assert.equal(colors.get(b), "violet", "the next hue after rose and plum");
  // A chosen color wins, and the project it belonged to moves on.
  const picked = groupColors([a, c], (k) => (k === c ? "rose" : null));
  assert.equal(picked.get(c), "rose");
  assert.equal(picked.get(a), "plum");
  // The grey only when chosen; past every hue, own colors repeat.
  assert.equal(groupColors([a], () => "grey").get(a), "grey");
  const all = [...groupColors(hues.flatMap((h) => of(h).slice(0, 2)), none).values()];
  assert.deepEqual(hues.map((h) => all.filter((x) => x === h).length), hues.map(() => 2));
});
