// Backend-neutral trace model. Adapters (Codex today, Claude Code next) turn a
// session's raw log into these records; the analyzer only reads these.

export type Category =
  | "edit"
  | "release"
  | "test"
  | "build"
  | "cloud"
  | "ci"
  | "vcs"
  | "coordinate"
  | "web"
  | "read"
  | "wait"
  // A request with no tool call: a progress update or final answer.
  | "message"
  | "other";

export interface Command {
  text: string;
  segments: Segment[];
  exitCode?: number;
  // Start to finish, including time spent running in the background while the
  // agent polled it.
  wallSeconds?: number;
  outputTokens: number;
  // How many times the agent polled this command before it finished.
  polls: number;
}

export interface Segment {
  text: string;
  category: Category;
  // Files a read or write segment names, as written in the command.
  files: string[];
  writes: boolean;
}

export interface PatchFile {
  path: string;
  op: "add" | "update" | "delete";
  added: number;
  removed: number;
}

export interface ToolCall {
  at: number;
  callId: string;
  // Every nested tool the call ran, in source order (exec_command, apply_patch,
  // write_stdin, clock__curr_time, send_message, ...).
  tools: string[];
  commands: Command[];
  patches: PatchFile[];
  emptyPolls: number;
  // Session ids of background commands this call polled, in source order.
  polled: number[];
  outputTokens: number;
  wallSeconds?: number;
  doneAt?: number;
}

export interface Request {
  at: number;
  turnId: string;
  input: number;
  cached: number;
  output: number;
  reasoning: number;
  calls: ToolCall[];
}

export interface Note {
  at: number;
  text: string;
}

export interface Trace {
  id: string;
  agentPath: string;
  parentId?: string;
  model?: string;
  cwd?: string;
  path: string;
  startedAt: number;
  endedAt: number;
  requests: Request[];
  // Index into `requests` of the first request after each compaction.
  compactions: number[];
  prompts: Note[];
  updates: Note[];
  credits?: { first: number; last: number; windowFirst: number; windowLast: number };
}
