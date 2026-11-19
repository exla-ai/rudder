// How the Cmd+K palette (command-palette.js) orders its rows for what is
// typed. No imports: tests load it in node.
//
// A row is { label, keywords?, searchOnly? }. With nothing typed the rows keep
// their order, without the searchOnly ones (per-project variants, older
// agents). Typed text matches, best first: the whole label, the label's
// start, the start of a word in it, every typed word starting a word of the
// label or its keywords ("rud term", a project name for its agents), then the
// label containing the text. Letters scattered through a label do not match.
// Ties keep the given order. Returns the matching rows.

const words = (s) => s.split(/[^\p{L}\p{N}]+/u).filter(Boolean);

function tier(row, q, qWords) {
  const label = row.label.toLowerCase();
  if (label === q) return 0;
  if (label.startsWith(q)) return 1;
  const labelWords = words(label);
  if (labelWords.some((w) => w.startsWith(q))) return 2;
  const all = [...labelWords, ...(row.keywords || []).flatMap((k) => words(String(k).toLowerCase()))];
  if (qWords.length && qWords.every((qw) => all.some((w) => w.startsWith(qw)))) return 3;
  return label.includes(q) ? 4 : -1;
}

export function rankCommands(rows, query) {
  const q = String(query || "").trim().toLowerCase().replace(/\s+/g, " ");
  if (!q) return rows.filter((r) => !r.searchOnly);
  const qWords = words(q);
  const hits = [];
  rows.forEach((row, at) => {
    const t = tier(row, q, qWords);
    if (t >= 0) hits.push({ row, t, at });
  });
  hits.sort((x, y) => x.t - y.t || x.at - y.at);
  return hits.map((x) => x.row);
}
