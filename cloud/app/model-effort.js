// Antigravity advertises each available effort as its own model ID.
export function groupAntigravityEfforts(models) {
  const rows = [];
  const groups = new Map();
  for (const row of models) {
    const variant = /^(.*)-(low|medium|high|xhigh|max)$/.exec(row.value);
    const label = /^(.*) \((Low|Medium|High|XHigh|Extra(?: High)?|Max)\)$/i.exec(row.label);
    if (!variant || !label || label[2].toLowerCase().replace(/^extra(?: high)?$/, "xhigh") !== variant[2]) {
      rows.push(row);
      continue;
    }
    const [, family, effort] = variant;
    let group = groups.get(family);
    if (!group) {
      // `family` is the id without the effort, which the picker chooses apart.
      group = { ...row, family, label: label[1], efforts: [], variants: {} };
      groups.set(family, group);
      rows.push(group);
    }
    group.efforts.push(effort);
    group.variants[effort] = row;
  }
  return rows;
}

// Antigravity meters its models in pools, each with its own allowance, named
// as agy names them: "Gemini" (Neon, Hydrogen, Flash), "Claude and GPT", and
// pools of one model family such as "Gemini 4 Argon". The pool among `pools`
// ([{ name }]) that `model` (an agy model id) draws on, or null when none of
// them is it.
export function antigravityPoolOf(model, pools) {
  const id = String(model || "").toLowerCase();
  if (/^(claude|gpt)/.test(id)) return pools.find((pool) => /claude|gpt/i.test(pool.name)) ?? null;
  // "gemini-4-argon-low" is Argon; the orchestrators are named after theirs,
  // the plain one being Carbon's.
  const family = /^gemini-pro-orchestrator(?:-([a-z]+))?$/.exec(id)?.[1]
    ?? (id === "gemini-pro-orchestrator" ? "carbon" : /^gemini-[\d.]+-([a-z]+)/.exec(id)?.[1]);
  if (!family) return null;
  const own = pools.find((pool) => pool.name.toLowerCase().split(/\s+/).includes(family));
  return own ?? pools.find((pool) => pool.name === "Gemini") ?? null;
}

export function matchesModel(row, model) {
  return row.value === model || Object.values(row.variants ?? {}).some((variant) => variant.value === model);
}

export function variantEffort(row, model) {
  return Object.entries(row?.variants ?? {}).find(([, variant]) => variant.value === model)?.[0];
}

export function modelRequest(row, effort) {
  if (!row.variants) return { model: row.value, effort, name: row.label };
  const variant = row.variants[effort];
  return { model: variant.value, effort: null, name: variant.label };
}
