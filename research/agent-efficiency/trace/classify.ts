// Shell command classification. Heuristic by design: it reads what the agent
// typed, not what the command did, so every category is an approximation.
import type { Category, Segment } from "./model.ts";

const WAIT = new Set(["tail", "sleep", "df", "du", "ps", "pgrep", "pidof", "date", "uptime", "free", "wait"]);
const READ = new Set([
  "sed", "cat", "head", "less", "rg", "grep", "ls", "find", "fd", "wc", "jq", "file", "stat", "tree",
  "awk", "nl", "diff", "cmp", "readlink", "realpath", "pwd", "which", "sort", "uniq", "cut", "column",
  "xxd", "od", "strings", "sha256sum", "shasum", "md5sum", "basename", "dirname", "env", "printenv",
]);
const FS_WRITE = new Set(["mv", "cp", "rm", "mkdir", "rmdir", "touch", "chmod", "chown", "ln", "install", "tar", "unzip", "zip", "truncate", "tee", "patch"]);
const CLOUD = new Set(["flyctl", "fly", "curl", "wget", "ssh", "scp", "aws", "tailscale", "gcloud", "kubectl", "nc"]);
const BUILD = new Set(["tsc", "rustc", "make", "cmake", "depot", "rustup", "xcodebuild", "swift", "cc", "clang", "gcc"]);
const PREFIX = new Set(["timeout", "nice", "time", "command", "exec", "sudo", "stdbuf", "setsid", "nohup", "xargs"]);
const SKIP = new Set(["cd", "export", "set", "source", ".", "unset", "trap", "true", "false", "exit", "return", "local", "fi", "done", "esac", "for", "while", "case", "umask", "{", "}"]);
const KEYWORD = new Set(["then", "do", "else", "if", "elif", "!"]);
const VCS_READ = new Set(["status", "diff", "log", "show", "blame", "ls-files", "grep", "rev-parse", "branch", "describe", "cat-file", "ls-tree", "merge-base", "op", "evolog", "st"]);
const WRITE_CODE = /write_text|write_bytes|open\([^)]*['"][wa]b?['"]|writeFileSync|appendFileSync|fs\.write|\.write\(|shutil\.(copy|move)|os\.(remove|rename|replace)|unlink\(/;
const TESTY = /test|qualif|smoke|verify|selftest|canary|proof|check/i;

export function splitShell(script: string): { text: string; heredoc: string }[] {
  const out: { text: string; heredoc: string }[] = [];
  const lines = script.split("\n");
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    const heredoc = /<<-?\s*['"]?([A-Za-z_][A-Za-z0-9_]*)['"]?/.exec(line);
    let body = "";
    if (heredoc) {
      const end = heredoc[1];
      const bodyLines: string[] = [];
      while (i + 1 < lines.length && lines[i + 1].trim() !== end) bodyLines.push(lines[++i]);
      i++;
      body = bodyLines.join("\n");
    }
    const parts = splitLine(line);
    parts.forEach((text, n) => out.push({ text, heredoc: heredoc && n === parts.length - 1 ? body : "" }));
  }
  return out.filter((part) => part.text.trim());
}

// Splits on ; && || outside quotes. Pipelines stay one segment.
function splitLine(line: string): string[] {
  const parts: string[] = [];
  let current = "";
  let quote = "";
  for (let i = 0; i < line.length; i++) {
    const ch = line[i];
    if (quote) {
      if (ch === "\\" && quote === '"') { current += ch + (line[++i] ?? ""); continue; }
      if (ch === quote) quote = "";
      current += ch;
      continue;
    }
    if (ch === "'" || ch === '"') { quote = ch; current += ch; continue; }
    if (ch === "#" && (i === 0 || /\s/.test(line[i - 1]))) break;
    const two = line.slice(i, i + 2);
    if (ch === ";" || two === "&&" || two === "||") {
      parts.push(current);
      current = "";
      if (two === "&&" || two === "||") i++;
      continue;
    }
    current += ch;
  }
  parts.push(current);
  return parts.map((part) => part.trim()).filter(Boolean);
}

export function words(text: string): string[] {
  const out: string[] = [];
  const re = /'([^']*)'|"((?:[^"\\]|\\.)*)"|(\S+)/g;
  for (let m = re.exec(text); m; m = re.exec(text)) out.push(m[1] ?? m[2] ?? m[3]);
  return out;
}

// The program a segment runs, after env assignments, wrappers like timeout and
// shell keywords. Pipelines are keyed by their first program.
export function programOf(text: string): { prog: string; args: string[] } {
  const argv = words(text.split(/\s\|\s/)[0]);
  while (argv.length && (/^[A-Za-z_][A-Za-z0-9_]*=/.test(argv[0]) || PREFIX.has(argv[0]) || KEYWORD.has(argv[0]))) {
    const prefix = argv.shift()!;
    if (prefix === "timeout" && /^\d/.test(argv[0] ?? "")) argv.shift();
    if (prefix === "stdbuf" && (argv[0] ?? "").startsWith("-")) argv.shift();
  }
  return { prog: (argv[0] ?? "").split("/").pop() ?? "", args: argv.slice(1) };
}

export function classifySegment(text: string, heredoc = ""): Segment {
  const pipeline = text.split(/\s\|\s/)[0];
  const { prog, args } = programOf(text);
  const seg = (category: Category, files: string[] = [], writes = false): Segment => ({ text, category, files, writes });
  if (!prog || SKIP.has(prog) || prog.startsWith("#")) return seg("other");

  if (prog === "git" || prog === "jj") {
    const rest = args.filter((arg, n) => !(arg.startsWith("-") || args[n - 1] === "-C" || args[n - 1] === "-R"));
    const sub = rest[0] ?? "";
    if (sub === "tag" || (sub === "push" && args.some((arg) => arg === "--tags" || /^v\d/.test(arg)))) return seg("release");
    if (sub === "ls-remote") return seg("ci");
    if (sub === "diff" && args.includes("--check")) return seg("test");
    return seg(VCS_READ.has(sub) ? "read" : "vcs");
  }
  if (prog === "npm" || prog === "pnpm" || prog === "yarn" || prog === "bun" || prog === "npx") {
    const sub = args[0] === "run" ? args[1] ?? "" : args[0] ?? "";
    if (sub === "version" || sub === "publish") return seg("release");
    if (sub === "view" || sub === "info") return seg("ci");
    if (/test/.test(sub) || args.includes("vitest")) return seg("test");
    return seg("build");
  }
  if (prog === "cargo") {
    const sub = args.find((arg) => !arg.startsWith("+") && !arg.startsWith("-")) ?? "";
    return seg(sub === "test" || sub === "nextest" || sub === "bench" ? "test" : "build");
  }
  if (prog === "node" || prog === "python3" || prog === "python" || prog === "bash" || prog === "sh" || prog === "deno" || prog === "tsx") {
    const code = heredoc + " " + (args.includes("-e") || args.includes("-c") ? args.join(" ") : "");
    if (WRITE_CODE.test(code)) return seg("edit", quotedPaths(code), true);
    if (args.includes("--test")) return seg("test");
    if (args.includes("--check")) return seg("build");
    if (args.includes("--version") || args.includes("-v")) return seg("read");
    const script = args.find((arg) => !arg.startsWith("-")) ?? "";
    if (TESTY.test(script)) return seg("test");
    if (/build|deploy|release|publish/.test(script)) return seg("build");
    // Inline scripts that write nothing are inspection: parsing JSON, hashing, listing.
    if (heredoc || args.includes("-c") || args.includes("-e") || script === "-") return seg("read", quotedPaths(code));
    return seg("other");
  }
  if (prog === "test" || prog === "[" || prog === "[[") return seg("read", pathArgs(args));
  if (args.includes("--version") || (prog === "command" && args[0] === "-v")) return seg("read");
  if (prog === "rustfmt" || prog === "prettier" || prog === "eslint") return seg("build");
  if (prog === "gh") return seg(args[0] === "release" ? "release" : "ci");
  if (prog === "docker" || prog === "podman") return seg(args[0] === "run" ? "test" : "build");
  if (CLOUD.has(prog)) return seg("cloud");
  if (BUILD.has(prog)) return seg("build");
  if ((prog === "sed" || prog === "perl") && args.some((arg) => /^-[a-z]*i/.test(arg))) return seg("edit", pathArgs(args), true);
  if (prog === "sha256sum" && args.includes("-c")) return seg("test");
  if ((prog === "cat" || prog === "echo" || prog === "printf") && /(^|[^2&])>\s*[^&\s]/.test(pipeline)) {
    return seg("edit", [pipeline.split(">").pop()!.trim().split(/\s/)[0]], true);
  }
  if (FS_WRITE.has(prog)) return seg("edit", pathArgs(args), true);
  if (WAIT.has(prog)) return seg("wait", pathArgs(args));
  if (prog === "kill" || prog === "pkill") return seg("other");
  if (READ.has(prog)) {
    const files = pathArgs(args);
    // Reading a build or test log is checking on a background job.
    if (files.length && files.every((file) => /\.log$|\.out$/.test(file))) return seg("wait", files);
    return seg("read", files);
  }
  return seg("other");
}

function pathArgs(args: string[]): string[] {
  return args.filter((arg) =>
    !arg.startsWith("-") && !/[|()\\*?\[\]{}^$<>]/.test(arg) && !/^\d+(,\d+)?p?$/.test(arg) &&
    (arg.includes("/") || /\.[A-Za-z0-9]{1,8}$/.test(arg)) && !/^https?:/.test(arg));
}

function quotedPaths(code: string): string[] {
  return [...code.matchAll(/['"]([^'"\s]+\/[^'"\s]+|[^'"\s/]+\.[A-Za-z0-9]{1,8})['"]/g)].map((m) => m[1]);
}

const RANK: Category[] = ["edit", "release", "test", "build", "cloud", "ci", "vcs", "coordinate", "web", "read", "wait", "message", "other"];

// One label per model request: the most consequential thing it did.
export function dominant(categories: Category[]): Category {
  if (!categories.length) return "other";
  const meaningful = categories.filter((category) => category !== "other");
  const pool = meaningful.length ? meaningful : categories;
  return RANK.find((category) => pool.includes(category)) ?? "other";
}

export function toolCategory(tool: string): Category | null {
  if (tool === "clock__curr_time" || tool === "wait_agent") return "wait";
  if (/send_message|spawn_agent|followup_task|list_agents|close_agent|resume_agent/.test(tool)) return "coordinate";
  if (tool.startsWith("web__") || tool === "web_search") return "web";
  if (/github/.test(tool)) return "ci";
  if (tool === "apply_patch") return "edit";
  return null;
}

// Reports quote commands, so strip anything that looks like a credential first.
export function redact(text: string): string {
  return text
    .replace(/\bFlyV1\s+\S+/g, "<redacted>")
    .replace(/\b([A-Za-z0-9_]*(?:TOKEN|SECRET|PASSWORD|PASSWD|API_KEY|AUTH|CREDENTIAL)[A-Za-z0-9_]*)=("[^"]*"|'[^']*'|\S+)/gi, "$1=<redacted>")
    .replace(/(authorization:\s*(?:bearer|token|basic)?\s*)\S+/gi, "$1<redacted>")
    .replace(/\b(?:gh[pousr]_|github_pat_|sk-|sk_live_|xox[abpr]-|FlyV1\s+fm\d_|AKIA)[A-Za-z0-9_\-+/=,]{8,}/g, "<redacted>")
    .replace(/\b[A-Za-z0-9+/_-]{40,}={0,2}/g, (match) => (/^[0-9a-f]+$/.test(match) ? match : "<redacted>"));
}

export function signature(text: string): string {
  return redact(text).replace(/\s+/g, " ").replace(/^(?:[A-Za-z_][A-Za-z0-9_]*=\S*\s+)+/, "").replace(/[0-9a-f]{12,}/g, "<hash>").replace(/\/tmp\/\S+/g, "<tmp>").slice(0, 120);
}
