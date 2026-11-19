// The files an agent's tool calls read, change or find, so the app's tool
// cards open them beside the chat (cloud/app/tool-files.js, file-link.js).
// Each harness records them its own way:
//   Claude Code: Read, Edit, MultiEdit, Write and NotebookEdit name the file
//     in their input, an Edit's result has the lines it changed
//     (`toolUseResult.structuredPatch`), and Grep and Glob list paths in
//     their output.
//   Codex: a patch (apply_patch, alone or in a code-mode `exec` script)
//     reports each file it changed as a FileChange item with a unified diff,
//     while its call is still open.
//   OpenCode: read, edit, write and apply_patch tool parts, whose metadata
//     has the diff, and grep and glob, which list paths in their output.
//
// A tool's `files` are [{ path, line, end }]: the path as the agent wrote it
// (absolute, or relative to its workspace), the line to open at (0 for the
// top) and the last line a change touched (0 when it is `line` or unknown).
// A search's `found` are [{ row, text, path, line }]: on output row `row`,
// `text` names `path`, at `line` when the output gives one. A failed call
// names nothing.

const SEARCH_TIMED_OUT = "The search stopped before it finished";

/** A file a tool call names. */
export function fileAt(path, line = 0, end = 0) {
  return { path, line, end: end > line ? end : 0 };
}

const named = (value) => (typeof value === "string" && value.trim() !== "" ? value : null);
const lineNumber = (value) => (Number.isInteger(value) && value > 0 ? value : 0);

/** Set `files` and `found` on a chat tool message's `tool`, or clear them. */
export function setToolFiles(tool, { files = [], found = [] }) {
  if (files.length) tool.files = files;
  else delete tool.files;
  if (found.length) tool.found = found;
  else delete tool.found;
}

// ---------------------------------------------------------------------------
// Claude Code
// ---------------------------------------------------------------------------

/** The files a Claude Code call names when it is made. A Write's file may
 * not exist until its result is in. */
export function claudeCallFiles(name, input) {
  if (!input || typeof input !== "object") return [];
  if (name === "Read" && named(input.file_path)) return [fileAt(input.file_path, lineNumber(input.offset))];
  if ((name === "Edit" || name === "MultiEdit") && named(input.file_path)) return [fileAt(input.file_path)];
  if (name === "NotebookEdit" && named(input.notebook_path)) return [fileAt(input.notebook_path)];
  return [];
}

/**
 * A Claude Code call's files once its result is in: an Edit or MultiEdit at
 * the lines it changed, a Grep's or Glob's paths from its output (`text`, of
 * which the chat shows the first `limit` characters).
 */
export function claudeResultFiles(name, input, { isError, result, text, limit }) {
  if (isError) return {};
  const files = name === "Write" && named(input?.file_path) ? [fileAt(input.file_path)] : claudeCallFiles(name, input);
  if ((name === "Edit" || name === "MultiEdit") && files.length && Array.isArray(result?.structuredPatch)) {
    const hunk = result.structuredPatch[0];
    if (hunk && Array.isArray(hunk.lines)) {
      const { line, end } = hunkLines(hunk.newStart, hunk.lines);
      files[0] = fileAt(files[0].path, line, end);
    }
  }
  const found = name === "Grep" ? grepFound(input, text) : name === "Glob" ? globFound(text) : [];
  return { files, found: shownRows(found, text, limit) };
}

// Grep: "Found N files" then a path a row; `path:line:text` rows (or
// `line:text` when it searched one file) for content; `path:count` rows for
// counts. A note the search timed out can come first.
function grepFound(input, text) {
  const mode = input?.output_mode ?? "files_with_matches";
  const rows = String(text).split("\n");
  const found = [];
  if (mode === "files_with_matches") {
    const header = rows.findIndex((row) => /^Found \d+ files?\b/.test(row));
    if (header < 0) return [];
    rows.forEach((row, at) => {
      if (at > header && row) found.push({ row: at, text: row, path: row, line: 0 });
    });
    return found;
  }
  if (mode === "content") {
    const single = named(input?.path);
    rows.forEach((row, at) => {
      const match = /^([^:\s][^:]*):(\d+):/.exec(row);
      if (match) {
        found.push({ row: at, text: `${match[1]}:${match[2]}`, path: match[1], line: Number(match[2]) });
        return;
      }
      const own = single ? /^(\d+):/.exec(row) : null;
      if (own) found.push({ row: at, text: own[1], path: single, line: Number(own[1]) });
    });
    return found;
  }
  if (mode === "count") {
    rows.forEach((row, at) => {
      const match = /^(.+):\d+$/.exec(row);
      if (match) found.push({ row: at, text: match[1], path: match[1], line: 0 });
    });
  }
  return found;
}

// Glob: a path a row, after a note the search timed out and before one that
// more were not listed ("(Results are truncated ...)").
function globFound(text) {
  const found = [];
  String(text).split("\n").forEach((row, at) => {
    if (row && row !== "No files found" && !row.startsWith("(") && !row.startsWith(SEARCH_TIMED_OUT)) {
      found.push({ row: at, text: row, path: row, line: 0 });
    }
  });
  return found;
}

// ---------------------------------------------------------------------------
// Codex
// ---------------------------------------------------------------------------

/** The files a Codex FileChange item changed, each at its first change. A deleted file has nothing to open. */
export function codexChangedFiles(item) {
  if (item?.status !== "completed" || !item.changes || typeof item.changes !== "object") return [];
  const files = [];
  for (const [file, change] of Object.entries(item.changes)) {
    if (change?.type === "add") files.push(fileAt(file));
    else if (change?.type === "update") {
      const { line, end } = diffLines(change.unified_diff);
      files.push(fileAt(named(change.move_path) ?? file, line, end));
    }
  }
  return files;
}

/** `files` with `more` added, one entry a path (the first change to a file is where it opens). */
export function withFiles(files, more) {
  const out = [...files];
  for (const file of more) {
    if (!out.some((f) => f.path === file.path)) out.push(file);
  }
  return out;
}

// ---------------------------------------------------------------------------
// OpenCode
// ---------------------------------------------------------------------------

/** The files an OpenCode tool part names, given the output the chat shows of it (`text`, `limit`). */
export function opencodeFiles(part, { text, limit }) {
  const state = part?.state ?? {};
  if (state.status === "error") return {};
  const input = state.input ?? {};
  const done = state.status === "completed";
  const metadata = state.metadata ?? {};
  switch (part.tool) {
    case "read":
      // A read of a folder lists it: there is no file to open.
      return done && metadata.display?.type === "file" && named(input.filePath) ? { files: [fileAt(input.filePath, lineNumber(input.offset))] } : {};
    case "write":
      return done && named(input.filePath) ? { files: [fileAt(input.filePath)] } : {};
    case "edit": {
      if (!named(input.filePath)) return {};
      const { line, end } = done ? diffLines(metadata.diff) : { line: 0, end: 0 };
      return { files: [fileAt(input.filePath, line, end)] };
    }
    case "apply_patch":
      if (!done || !Array.isArray(metadata.files)) return {};
      return {
        files: metadata.files
          .filter((f) => f?.type !== "delete" && named(f?.movePath ?? f?.filePath))
          .map((f) => {
            const { line, end } = f.type === "add" ? { line: 0, end: 0 } : diffLines(f.patch);
            return fileAt(f.movePath ?? f.filePath, line, end);
          }),
      };
    case "grep":
      return done ? { found: shownRows(opencodeGrepFound(text), text, limit) } : {};
    case "glob":
      return done ? { found: shownRows(opencodeGlobFound(text), text, limit) } : {};
    default:
      return {};
  }
}

// "Found N matches", then each file's path and a colon, its matches below
// it as "  Line 12: text". The path opens at its first match.
function opencodeGrepFound(text) {
  const found = [];
  let file = null;
  String(text).split("\n").forEach((row, at) => {
    const match = /^ {2}Line (\d+): /.exec(row);
    if (match && file) {
      const line = Number(match[1]);
      if (!file.line) file.line = line;
      found.push({ row: at, text: `Line ${match[1]}`, path: file.path, line });
      return;
    }
    if (at > 0 && row.endsWith(":") && /^[^\s(]/.test(row)) {
      file = { row: at, text: row.slice(0, -1), path: row.slice(0, -1), line: 0 };
      found.push(file);
    }
  });
  return found;
}

// A path a row, each resolved to absolute; "No files found" or a note
// that more were not listed are not.
function opencodeGlobFound(text) {
  const found = [];
  String(text).split("\n").forEach((row, at) => {
    if (row.startsWith("/")) found.push({ row: at, text: row, path: row, line: 0 });
  });
  return found;
}

// ---------------------------------------------------------------------------
// Shared
// ---------------------------------------------------------------------------

/**
 * The lines a diff hunk changed in the new file, from its first line
 * (`start`) and its lines (" " kept, "-" removed, "+" added): the first and
 * last added line, or where lines were only removed.
 */
export function hunkLines(start, lines) {
  let at = Number(start) || 0;
  let line = 0;
  let end = 0;
  for (const text of lines) {
    const mark = String(text)[0];
    if (mark === "+") {
      line ||= at;
      end = at;
      at += 1;
    } else if (mark === "-") {
      line ||= at;
      end = Math.max(end, at);
    } else if (mark === " ") {
      at += 1;
    }
  }
  return { line, end };
}

// A unified diff's first hunk: the lines after its first "@@ -a,b +c,d @@".
function diffLines(diff) {
  const rows = String(diff ?? "").split("\n");
  const head = rows.findIndex((row) => /^@@ -\d+(?:,\d+)? \+\d+(?:,\d+)? @@/.test(row));
  if (head < 0) return { line: 0, end: 0 };
  const start = Number(/\+(\d+)/.exec(rows[head])[1]);
  const next = rows.findIndex((row, at) => at > head && row.startsWith("@@"));
  return hunkLines(start, rows.slice(head + 1, next < 0 ? rows.length : next));
}

// The rows the chat shows whole: output past `limit` characters is cut, so
// the row the cut falls in and the ones after it are left out.
function shownRows(found, text, limit) {
  const value = String(text ?? "");
  if (value.length <= limit) return found;
  const whole = value.slice(0, limit).split("\n").length - 1;
  return found.filter((f) => f.row < whole);
}
