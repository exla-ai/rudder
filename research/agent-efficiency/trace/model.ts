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
  // Every prompt token the request carried, cached or not.
  input: number;
  cached: number;
  // Prompt tokens written to the cache by this request (Claude only).
  cacheWrite?: number;
  output: number;
  reasoning: number;
  // Request start to last streamed block, when the log records both (Claude).
  modelSeconds?: number;
  calls: ToolCall[];
}

// Instruction files (CLAUDE.md, AGENTS.md, memory) the backend put into the
// prompt at session start or after a compaction.
export interface ContextLoad {
  at: number;
  files: { path: string; kind: string; bytes: number }[];
}

export interface Note {
  at: number;
  text: string;
}

export interface Trace {
  id: string;
  backend: "codex" | "claude";
  agentPath: string;
  parentId?: string;
  model?: string;
  effort?: string;
  // How the backend was started: codex-tui, codex_exec, cli, sdk-cli, ...
  entrypoint?: string;
  cwd?: string;
  path: string;
  startedAt: number;
  endedAt: number;
  requests: Request[];
  // Index into `requests` of the first request after each compaction.
  compactions: number[];
  // Context size each compaction started from and how long it took, when the
  // log says (Claude).
  compactedFrom?: number[];
  compactSeconds?: number[];
  contextLoads?: ContextLoad[];
  // Rendered bytes the backend injected per attachment kind (Claude).
  attachments?: Record<string, { count: number; bytes: number }>;
  prompts: Note[];
  updates: Note[];
  credits?: { first: number; last: number; windowFirst: number; windowLast: number };
}
