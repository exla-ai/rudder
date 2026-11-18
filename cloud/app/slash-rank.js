// How the chat's "/" menu orders its rows for what is typed, as the agent's
// own CLI does (slash-menu.js). No imports: tests load it in node.

// What a row shows: Claude names an MCP prompt "server:prompt (MCP)".
export const labelOf = (c) => c.label ?? c.name;

// Codex: exact, then prefix, in its own order, and its alias rows only once
// something is typed. Claude Code ranks its search hits by: an exact name,
// an exact alias, names starting with the query (shortest first), aliases
// starting with it, then how well the rest match: a word of the name, the
// name containing it, a word of the description. Claude finds those with a
// fuzzy search; the app takes the plain matches. A row found by its alias
// shows the alias, as Claude does. Returns [{ command, alias }].
export function rank(commands, query, backend) {
  const q = query.toLowerCase();
  if (!q) return commands.filter((c) => backend !== "codex" || !c.alias).map((command) => ({ command, alias: null }));
  if (backend === "codex") {
    const exact = commands.filter((c) => c.name.toLowerCase() === q);
    const prefix = commands.filter((c) => c.name.toLowerCase() !== q && c.name.toLowerCase().startsWith(q));
    return [...exact, ...prefix].map((command) => ({ command, alias: null }));
  }
  const word = q.replace(/[^a-z0-9]/g, "");
  const hits = [];
  commands.forEach((c, at) => {
    const names = [c.name.toLowerCase(), labelOf(c).toLowerCase()];
    const aliases = (c.aliases || []).map((n) => n.toLowerCase());
    const prefixed = names.filter((n) => n.startsWith(q)).map((n) => n.length);
    const alias = aliases.find((n) => n.startsWith(q)) ?? null;
    const fit = prefixed.length || alias ? 0
      : names.some((n) => n.split(/[\s:\-_.]+/).some((w) => w.startsWith(q))) ? 1
        : names.some((n) => n.includes(q)) ? 2
          : word && c.description.toLowerCase().split(/[^a-z0-9]+/).some((w) => w.startsWith(word)) ? 3 : -1;
    if (fit < 0) return;
    const key = [names.includes(q) ? 0 : 1, aliases.includes(q) ? 0 : 1, prefixed.length ? Math.min(...prefixed) : Infinity, alias ? alias.length : Infinity, fit, at];
    hits.push({ command: c, alias, key });
  });
  hits.sort((a, b) => {
    for (let i = 0; i < a.key.length; i++) if (a.key[i] !== b.key[i]) return a.key[i] < b.key[i] ? -1 : 1;
    return 0;
  });
  return hits.map(({ command, alias }) => ({ command, alias }));
}
