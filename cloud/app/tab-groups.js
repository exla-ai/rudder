// Tabs grouped by project (tabs.js). A tab's group is the project it shows,
// worked out from the tab and never stored: `keyOf(tab)` gives that
// project's workspace id, or null for a page with no project of yours (the
// home screen, Settings, All agents, a teammate's agent). The list keeps each
// group's tabs side by side: the tabs with no project first, then one run per
// project, in the order the projects first appear. Each project has a color
// (groupColors).

/** The list in that order, each group's tabs keeping their own order. */
export function arrange(tabs, keyOf) {
  const groups = new Map([[null, []]]);
  for (const t of tabs) {
    const key = keyOf(t);
    if (!groups.has(key)) groups.set(key, []);
    groups.get(key).push(t);
  }
  return [...groups.values()].flat();
}

/**
 * Where a new tab of group `key` goes in an arranged list: after that group's
 * last tab. A project's first tab starts its group at the end of the list.
 */
export function placeFor(tabs, key, keyOf) {
  for (let i = tabs.length - 1; i >= 0; i--) if (keyOf(tabs[i]) === key) return i + 1;
  return key === null ? 0 : tabs.length;
}

/**
 * `tab` of an arranged list now shows another page. It goes to the end of its
 * project's group, or, as that project's first tab, stays near where it was.
 */
export function regroup(tabs, tab, keyOf) {
  const rest = tabs.filter((t) => t !== tab);
  const key = keyOf(tab);
  if (!rest.some((t) => keyOf(t) === key)) return arrange(tabs, keyOf);
  rest.splice(placeFor(rest, key, keyOf), 0, tab);
  return rest;
}

/** An arranged list's groups in order: [{ key, tabs }]. */
export function groupsOf(tabs, keyOf) {
  const out = [];
  for (const t of tabs) {
    const key = keyOf(t);
    if (!out.length || out.at(-1).key !== key) out.push({ key, tabs: [] });
    out.at(-1).tabs.push(t);
  }
  return out;
}

// A project's color, on its chip, under its tabs and beside it in the
// sidebar: soft hues kept apart from the state colors (tokens --group-<name>
// in styles.css). The account can choose one per project; the server keeps
// the same list (cloud/src/server.ts PROJECT_COLORS).
export const COLORS = ["rose", "plum", "violet", "slate", "sage", "olive", "clay", "grey"];
// A project nobody chose a color for gets one of the hues, never the grey.
const OWN = COLORS.slice(0, -1);

/** A project's own color, the same on every device: from its workspace id. */
export function ownColor(key) {
  let n = 2166136261;
  for (let i = 0; i < key.length; i++) n = Math.imul(n ^ key.charCodeAt(i), 16777619) >>> 0;
  return OWN[n % OWN.length];
}

/**
 * The color of each project in `keys` (the groups open in the strip): the one
 * chosen for it (`chosen(key)`, else null), or its own. Two open projects
 * share an own color only once every hue is taken: of those whose own color
 * a chosen one or another project already shows, each but the first by id
 * takes the next free hue.
 */
export function groupColors(keys, chosen) {
  const out = new Map();
  const taken = new Set();
  for (const key of keys) {
    const c = chosen(key);
    if (c) { out.set(key, c); taken.add(c); }
  }
  const later = [];
  for (const key of keys.filter((k) => !out.has(k)).sort()) {
    const c = ownColor(key);
    if (taken.has(c)) later.push(key);
    else { out.set(key, c); taken.add(c); }
  }
  for (const key of later) {
    const at = OWN.indexOf(ownColor(key));
    const c = OWN.map((_, i) => OWN[(at + i) % OWN.length]).find((x) => !taken.has(x)) || ownColor(key);
    out.set(key, c);
    taken.add(c);
  }
  return out;
}
