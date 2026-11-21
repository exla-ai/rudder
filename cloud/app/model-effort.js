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
      group = { ...row, label: label[1], efforts: [], variants: {} };
      groups.set(family, group);
      rows.push(group);
    }
    group.efforts.push(effort);
    group.variants[effort] = row;
  }
  return rows;
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
