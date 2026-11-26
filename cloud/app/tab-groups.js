// Tabs grouped by project (tabs.js). A tab's group is the project it shows,
// worked out from the tab and never stored: `keyOf(tab)` gives that
// project's workspace id, or null for a page with no project of yours (the
// home screen, Settings, All agents, a teammate's agent). The list keeps each
// group's tabs side by side: the tabs with no project first, then one run per
// project, in the order the projects first appear.

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
