// Agents closed on this device stay out of the polled state until it stops
// listing them (closeAgent in agent-menu.js). The dashboard deletes an
// agent's record before it answers, but Cloud's copy of the state can lag the
// answer by a publish: taken as it came, it brought a closed row back for
// seconds after "Agent closed".

/** How long after the dashboard's yes the state may still list a closed agent. */
export const CLOSE_AGREE_MS = 60_000;

/**
 * Drop the agents in `closing` (`${ws}|${run}` -> `{ title, answered }`,
 * `answered` the time the dashboard said yes, null while asking) from
 * `state`'s projects and its All agents project, and forget each answered
 * close the state no longer lists. Returns the closes the state still lists
 * CLOSE_AGREE_MS after the yes: those rows are shown again.
 */
export function hideClosed(state, closing, now) {
  const stuck = [];
  if (!closing.size) return stuck;
  const listed = new Set();
  for (const p of [...(state?.projects || []), state?.coordinator]) {
    if (!p?.agents) continue;
    p.agents = p.agents.filter((a) => {
      const k = `${p.workspaceId}|${a.id}`;
      listed.add(k);
      const c = closing.get(k);
      if (!c) return true;
      if (c.answered === null || now - c.answered < CLOSE_AGREE_MS) return false;
      closing.delete(k);
      stuck.push(c);
      return true;
    });
  }
  for (const [k, c] of closing) if (c.answered !== null && !listed.has(k)) closing.delete(k);
  return stuck;
}
