import { evidenceId, redact } from "./privacy.js";
import { boundedRows, editEvidence } from "./storage.js";
import { readJson } from "../util.js";
import type { JsonValue } from "../types.js";

export type LessonKind = "targeted_read" | "bounded_wait" | "affected_check";
const advice: Record<LessonKind, string> = {
  targeted_read: "Retrieve the relevant symbol or line range before repeating a read of unchanged content.",
  bounded_wait: "Subscribe once with a deadline and read the settled result. A timeout does not release worker admission.",
  affected_check: "Reuse a successful check only when its exact declared inputs and toolchain are unchanged. Required release gates still apply.",
};
export type LessonScope = { projectId: string; paths: string[]; sourceRevision: string; toolchain: string };
export type TraceFinding = {
  kind: LessonKind; traceId: string; stepIds: string[];
  outcome: { id: string; accepted: boolean; failedChecks: number; reviewRejected: boolean };
  scope: LessonScope; observedAt: number; repetitions: number; unchangedInputs: boolean;
};
export type Lesson = {
  schema: 1; id: string; kind: LessonKind; project: string; paths: string[];
  sourceRevision: string; toolchain: string; provenance: string[];
  createdAt: number; expiresAt: number; recheck: "revision_or_toolchain_change";
  authority: "advisory_only";
};

function pathsWithinRepo(paths: string[]): string[] {
  return [...new Set(paths.filter(p => p.length > 0 && p.length <= 160 && redact(p, 160) === p && !p.startsWith("/") && !p.includes("\\") && p.split("/").every(part => part !== ".." && part !== "." && part !== "") && /^[\w./-]+$/.test(p)))].slice(0, 8);
}

/** Derive only from bounded numeric findings and an authoritative accepted outcome.
 * The caller obtains acceptance from integration/review, never model prose. */
export function deriveLesson(finding: TraceFinding, now: number, ttlMs = 7 * 86400_000): Lesson | undefined {
  if (!Object.hasOwn(advice, finding.kind) || !Number.isFinite(now) || now < 0 || !Number.isFinite(ttlMs) || ttlMs <= 0 || ttlMs > 7 * 86400_000) return undefined;
  if (!Number.isFinite(finding.observedAt) || finding.observedAt < 0 || finding.observedAt > now || finding.observedAt + ttlMs <= now) return undefined;
  if (!finding.outcome.accepted || finding.outcome.failedChecks !== 0 || finding.outcome.reviewRejected || !finding.unchangedInputs || !Number.isSafeInteger(finding.repetitions) || finding.repetitions < 2) return undefined;
  const paths = pathsWithinRepo(finding.scope.paths);
  if (!paths.length || !finding.traceId || !finding.outcome.id || !finding.stepIds.length || !finding.scope.projectId || !finding.scope.sourceRevision || !finding.scope.toolchain) return undefined;
  const provenance = [evidenceId(finding.traceId), evidenceId(finding.outcome.id), ...finding.stepIds.slice(0, 6).map(evidenceId)];
  const project = evidenceId(finding.scope.projectId);
  const sourceRevision = evidenceId(finding.scope.sourceRevision);
  const toolchain = evidenceId(finding.scope.toolchain);
  const id = evidenceId(JSON.stringify([project, finding.kind, paths, sourceRevision, toolchain, provenance]));
  return { schema: 1, id, kind: finding.kind, project, paths, sourceRevision, toolchain, provenance, createdAt: finding.observedAt, expiresAt: finding.observedAt + ttlMs, recheck: "revision_or_toolchain_change", authority: "advisory_only" };
}

type LessonFile = { schema: 1; lessons: Lesson[]; rejected: string[] };
const digest = (value: unknown): value is string => typeof value === "string" && /^[a-f0-9]{64}$/.test(value);
function validLesson(value: unknown): value is Lesson {
  if (!value || typeof value !== "object") return false;
  const row = value as Lesson;
  const keys = new Set(["schema", "id", "kind", "project", "paths", "sourceRevision", "toolchain", "provenance", "createdAt", "expiresAt", "recheck", "authority"]);
  return Object.keys(row).every(k => keys.has(k)) && row.schema === 1 && Object.hasOwn(advice, row.kind)
    && [row.id, row.project, row.sourceRevision, row.toolchain].every(digest)
    && row.authority === "advisory_only" && row.recheck === "revision_or_toolchain_change"
    && Number.isFinite(row.createdAt) && row.createdAt >= 0 && Number.isFinite(row.expiresAt) && row.expiresAt > row.createdAt && row.expiresAt - row.createdAt <= 7 * 86400_000
    && Array.isArray(row.paths) && row.paths.length > 0 && row.paths.length <= 8 && row.paths.every(p => typeof p === "string") && pathsWithinRepo(row.paths).length === row.paths.length
    && Array.isArray(row.provenance) && row.provenance.length >= 3 && row.provenance.length <= 8 && row.provenance.every(digest);
}
function cleanFile(value: LessonFile | null): LessonFile {
  return { schema: 1, lessons: value?.schema === 1 && Array.isArray(value.lessons) ? value.lessons.filter(validLesson).slice(-128) : [], rejected: value?.schema === 1 && Array.isArray(value.rejected) ? value.rejected.filter(digest).slice(-512) : [] };
}

/** Shared retrieval for Claude and Codex. No backend-specific prompt or scheduler. */
export class ScopedLessonStore {
  readonly file: string;
  constructor(file: string) { this.file = file; }

  async learn(finding: TraceFinding, now: number, ttlMs?: number): Promise<Lesson | undefined> {
    const lesson = deriveLesson(finding, now, ttlMs);
    if (!lesson) return undefined;
    let stored: Lesson | undefined;
    await editEvidence<LessonFile>(this.file, current => {
      const clean = cleanFile(current);
      // An expired short-lived lesson cannot be renewed by replaying its old
      // evidence with the default TTL. A fresh accepted trace is a recheck.
      clean.rejected = [...new Set([...clean.rejected, ...clean.lessons.filter(row => row.expiresAt <= now).map(row => row.id)])].slice(-512);
      clean.lessons = clean.lessons.filter(row => row.expiresAt > now);
      // Saturation fails closed rather than evicting a rejection and later
      // resurrecting it. A new scoped store requires an explicit owner reset.
      if (clean.rejected.length < 512 && !clean.rejected.includes(lesson.id)) {
        // Re-observing the same evidence cannot extend its lifetime.
        if (!clean.lessons.some(row => row.id === lesson.id)) clean.lessons.push(lesson);
        clean.lessons = boundedRows(clean.lessons, 128, 128 * 1024);
        stored = clean.lessons.find(row => row.id === lesson.id);
      }
      return clean as unknown as JsonValue;
    });
    return stored;
  }

  async reject(id: string): Promise<void> {
    if (!digest(id)) throw new Error("invalid_lesson_id");
    await editEvidence<LessonFile>(this.file, current => {
      const clean = cleanFile(current);
      clean.lessons = clean.lessons.filter(row => row.id !== id);
      clean.rejected = [...new Set([...clean.rejected, id])].slice(-512);
      return clean as unknown as JsonValue;
    });
  }

  async retrieve(scope: LessonScope, now: number, options: { maxLessons?: number; maxBytes?: number } = {}): Promise<{ id: string; text: string; provenance: string[]; authority: "advisory_only" }[]> {
    const maxLessons = options.maxLessons ?? 3;
    const maxBytes = options.maxBytes ?? 2048;
    if (!Number.isSafeInteger(maxLessons) || maxLessons < 1 || maxLessons > 5 || !Number.isSafeInteger(maxBytes) || maxBytes < 128 || maxBytes > 4096 || !Number.isFinite(now)) throw new Error("invalid_retrieval_bounds");
    const file = cleanFile(await readJson<LessonFile>(this.file));
    const paths = pathsWithinRepo(scope.paths);
    const matches = file.lessons.filter(row => row.project === evidenceId(scope.projectId) && row.sourceRevision === evidenceId(scope.sourceRevision) && row.toolchain === evidenceId(scope.toolchain)
      && row.createdAt <= now && row.expiresAt > now && !file.rejected.includes(row.id)
      && row.paths.some(p => paths.some(q => p === q || q.startsWith(`${p}/`) || p.startsWith(`${q}/`))))
      .sort((a, b) => b.createdAt - a.createdAt || a.id.localeCompare(b.id));
    const result: { id: string; text: string; provenance: string[]; authority: "advisory_only" }[] = [];
    for (const row of matches) {
      if (result.length >= maxLessons) break;
      const item = { id: row.id, text: advice[row.kind], provenance: row.provenance, authority: "advisory_only" as const };
      if (Buffer.byteLength(JSON.stringify([...result, item])) <= maxBytes) result.push(item);
    }
    return result;
  }
}
