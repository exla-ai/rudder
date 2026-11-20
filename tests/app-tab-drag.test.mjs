import assert from "node:assert/strict";
import test from "node:test";
import { tabDragPosition } from "../cloud/app/tab-drag.js";

const items = [2, 101, 200, 299].map((left) => ({ left, center: left + 48 }));
const layout = { items, width: 96, right: 395, grab: 48 };
const strip = { left: 50, right: 447 };
const viewport = { clientWidth: 397, scrollWidth: 397, scrollLeft: 0 };

test("a tab switches slots as its center crosses a neighbor", () => {
  assert.deepEqual(tabDragPosition({ ...layout, from: 1, x: 297 }, strip, viewport), { to: 1, offset: 98 });
  assert.deepEqual(tabDragPosition({ ...layout, from: 1, x: 299 }, strip, viewport), { to: 2, offset: 100 });
  assert.deepEqual(tabDragPosition({ ...layout, from: 2, x: 198 }, strip, viewport), { to: 1, offset: -100 });
});

test("an off-center grab can reach both end slots when the strip finishes scrolling", () => {
  const bounds = { left: 50, right: 350 };
  const overflow = { ...viewport, clientWidth: 300 };
  const farRight = { ...layout, from: 0, grab: 95, x: 345 };
  assert.equal(tabDragPosition(farRight, bounds, overflow).to, 1);
  assert.deepEqual(tabDragPosition(farRight, bounds, { ...overflow, scrollLeft: 97 }), { to: 3, offset: 295 });
  assert.deepEqual(tabDragPosition({ ...layout, from: 3, grab: 0, x: 55 }, bounds, overflow), { to: 0, offset: -294 });
});
