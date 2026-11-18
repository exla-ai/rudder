// Claude Code's suggested next prompt for an agent, for the app's composer.
//
// Claude Code guesses the person's next prompt after a turn and shows it dim
// in its own prompt box. It keeps the guess in memory only: the session JSONL
// never has it. Rudder's Claude plugin (a `prompt.suggest` hook) saves each
// one it shows to <home>/.rudder/signals/<run>-suggestion.json as
// {"text": "...", "at": <ms>} and writes {"text": null} when a turn starts or
// the session ends. Codex has no such guess, so its agents have no file.

import fs from "node:fs";
import path from "node:path";

const MAX_TEXT = 2000;

export function suggestionFile(home, runId) {
  return path.join(home, ".rudder", "signals", `${runId}-suggestion.json`);
}

// { text, at } while Claude Code is showing a suggestion for this run, else null.
export function promptSuggestion(home, runId) {
  let body;
  try {
    body = fs.readFileSync(suggestionFile(home, runId), "utf8");
  } catch (error) {
    if (error.code === "ENOENT") return null;
    throw error;
  }
  let saved;
  try {
    saved = JSON.parse(body);
  } catch {
    // The plugin rewrites the file in place (its file API has no rename), so a
    // read can land mid-write. The next poll reads the finished file.
    return null;
  }
  const text = typeof saved?.text === "string" ? saved.text.trim() : "";
  if (!text) return null;
  if (!Number.isFinite(saved.at)) {
    throw new Error(`${suggestionFile(home, runId)}: a suggestion without its time`);
  }
  return { text: text.slice(0, MAX_TEXT), at: saved.at };
}
