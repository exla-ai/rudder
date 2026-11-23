// Chats closed on a dashboard (native agent_life.rs close_agent_now). A
// deleted chat's record moves to <repo>/.rudder/closed/<id>/run.json, with
// closed.json {closedAt: unix seconds} beside it, and stays a day with its
// conversation and workspace. The app's Show closed lists these, and its
// Reopen asks the dashboard to bring one back (steer `reopen`).

import fs from "node:fs";
import path from "node:path";
import { userTask } from "./transcripts.mjs";

/**
 * The closed chats of one repo, newest first: `{ id, title, backend, model,
 * effort, closedAt }`, `closedAt` in milliseconds. The dashboard moves the
 * record before it writes the stamp, so a chat closing this moment has no
 * stamp yet; it is listed on the next read.
 */
export function readClosedChats(repoDir) {
  const closedDir = path.join(repoDir, ".rudder", "closed");
  let entries;
  try {
    entries = fs.readdirSync(closedDir);
  } catch (error) {
    if (error.code === "ENOENT") return [];
    throw error;
  }
  const chats = [];
  for (const id of entries) {
    const dir = path.join(closedDir, id);
    let record;
    let stamp;
    try {
      record = JSON.parse(fs.readFileSync(path.join(dir, "run.json"), "utf8"));
      stamp = JSON.parse(fs.readFileSync(path.join(dir, "closed.json"), "utf8"));
    } catch {
      continue;
    }
    if (record?.id !== id || typeof stamp?.closedAt !== "number") continue;
    chats.push({
      id,
      title: (record.taskSummary || userTask(record.task) || id).toString().slice(0, 200),
      backend: record.backend ?? "claude",
      model: record.model ?? null,
      effort: record.effort ?? null,
      closedAt: stamp.closedAt * 1000,
    });
  }
  return chats.sort((a, b) => b.closedAt - a.closedAt);
}
