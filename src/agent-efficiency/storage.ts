import fs from "node:fs/promises";
import path from "node:path";
import { randomUUID } from "node:crypto";
import { readJson, withPathLock, writeJson } from "../util.js";
import type { JsonValue } from "../types.js";

export async function editEvidence<T>(file: string, edit: (current: T | null) => JsonValue): Promise<void> {
  const key = path.resolve(file);
  await fs.mkdir(path.dirname(key), { recursive: true, mode: 0o700 });
  // Reuse the repository's cross-process JSON lock. writeJson locks its own
  // destination, so stage to a unique sibling to avoid recursively locking key.
  // Both the stage and final artifact are private from their first publication.
  await withPathLock(key, async () => {
    const stage = `${key}.${randomUUID()}.evidence-stage`;
    try {
      await writeJson(stage, edit(await readJson<T>(key)), { mode: 0o600 });
      await fs.rename(stage, key);
    } finally {
      await fs.rm(stage, { force: true });
    }
  });
}

export function boundedRows<T>(rows: T[], maxRecords: number, maxBytes: number): T[] {
  const kept = rows.slice(-maxRecords);
  while (kept.length && Buffer.byteLength(JSON.stringify(kept)) > maxBytes) kept.shift();
  return kept;
}
