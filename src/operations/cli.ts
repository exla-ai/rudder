import { assertAliasCurrent, COMMAND_REGISTRY, commandCompletions, commandHelp } from "./registry.js";
import { deterministicTaskRoute } from "../task-routing.js";

type SurfaceFlags = {
  help?: boolean; json?: boolean; backend?: string; model?: string; workspace?: boolean;
  cloud?: boolean; continue?: boolean; delete?: boolean; version?: boolean;
};
type ParsedSurface = { command?: string; args: string[]; flags: SurfaceFlags };
type Alias = (typeof COMMAND_REGISTRY.aliases)[number] & { backend?: string; cloud?: boolean; bare_only?: boolean };

const PASSTHROUGH = new Set<string>([...COMMAND_REGISTRY.callbacks, ...COMMAND_REGISTRY.developer, "help", "completion", "version"]);
const PUBLIC: Record<string, string> = { ls: "runs", attach: "attach", logs: "logs", login: "login", logout: "logout", cloud: "cloud", update: "update" };
/** Old families: an unknown action there is a typo, never a task. */
const OLD_FAMILIES = new Set(["agent", "change", "project", "auth"]);

const stderr = (text: string) => { process.stderr.write(text); };

/** Map the public CLI and old spellings onto the dispatch adapters in main.ts.
 * Old spellings print one "use X instead" line and still run. Callbacks and
 * developer commands pass through byte for byte. */
export function applyCommandSurface<T extends ParsedSurface>(input: T, now = new Date(), rawArgv?: readonly string[], warn = stderr): T {
  const parsed = { ...input, args: [...input.args], flags: { ...input.flags } };
  const name = parsed.command;
  if (!name) {
    if (parsed.flags.continue && !parsed.flags.help && !parsed.flags.version) parsed.command = "handoff";
    return parsed;
  }
  if (name.startsWith("__") || PASSTHROUGH.has(name) || parsed.flags.help) return parsed;
  const delimiter = rawArgv?.indexOf("--") ?? -1;
  const flagArgv = delimiter >= 0 ? rawArgv!.slice(0, delimiter) : rawArgv ?? [];
  for (const alias of COMMAND_REGISTRY.flag_aliases) if (flagArgv.includes(alias.name)) assertAliasCurrent(alias, now);
  if (COMMAND_REGISTRY.retired.includes(name)) throw new Error(`rudder ${name} was removed; run rudder --help`);

  if (name in PUBLIC) { parsed.command = PUBLIC[name]; return parsed; }
  if (name === "stop") { parsed.command = parsed.flags.delete ? "delete" : "stop"; return parsed; }
  if (name === "doctor") {
    const action = parsed.args[0];
    if (!action) return parsed;
    const operation = COMMAND_REGISTRY.operations.find((entry) => entry.cli[0] === "doctor" && entry.cli[1] === action);
    if (!operation?.adapter) throw new Error(`Unknown doctor action: ${action}. Choose ${commandCompletions(["doctor"]).join(", ")}`);
    parsed.args.shift();
    parsed.command = operation.adapter;
    return parsed;
  }

  const aliases = COMMAND_REGISTRY.aliases as Alias[];
  const twoWord = parsed.args[0] ? aliases.find((entry) => entry.name === `${name} ${parsed.args[0]}`) : undefined;
  const alias = twoWord ?? aliases.find((entry) => entry.name === name && (!entry.bare_only || parsed.args.length === 0));
  if (alias) {
    assertAliasCurrent(alias, now);
    warn(`use ${alias.replacement} instead\n`);
    if (twoWord) parsed.args.shift();
    parsed.command = alias.adapter;
    if (alias.backend) {
      if (parsed.flags.backend && parsed.flags.backend !== alias.backend) throw new Error("Conflicting backend selection");
      parsed.flags.backend = alias.backend;
      if (alias.backend === "acpx" && parsed.args[0] === "codex") parsed.args.shift();
    }
    if (alias.cloud) parsed.flags.cloud = true;
    if (parsed.command === "dashboard" && parsed.args.length) parsed.command = "attach";
    if (parsed.command !== "run") return parsed;
  } else {
    if (OLD_FAMILIES.has(name)) throw new Error(`Unknown command: rudder ${[name, ...parsed.args.slice(0, 1)].join(" ")}. Run rudder --help`);
    if (name.startsWith("/") || name.startsWith("-")) throw new Error(`Unknown Rudder command: ${name}`);
    parsed.args.unshift(name);
  }

  // rudder "task" [--cloud] [--backend X] [--continue]
  if (parsed.flags.continue) { parsed.command = "handoff"; return parsed; }
  const task = parsed.args.join(" ").trim();
  if (!task) throw new Error('Missing task. Usage: rudder "fix the tests"');
  const route = deterministicTaskRoute(task);
  if (route.route !== "execute") throw new Error(`${route.reason}; run rudder and plan it in the dashboard`);
  if (parsed.flags.cloud && (parsed.flags.backend || parsed.flags.model)) {
    throw new Error("--cloud cannot keep --backend or --model yet; pick them in the dashboard");
  }
  parsed.flags.workspace = true;
  parsed.command = parsed.flags.cloud ? "sail" : "run";
  return parsed;
}

/** Called before setup, updater, jj discovery or any native launch. */
export function printCommandDiscovery(parsed: ParsedSurface, write = (text: string) => process.stdout.write(text)): boolean {
  if (parsed.command === "completion") {
    const completions = commandCompletions(parsed.args);
    write(parsed.flags.json ? `${JSON.stringify(completions)}\n` : `${completions.join("\n")}\n`);
    return true;
  }
  if (parsed.command === "help" || parsed.flags.help) {
    const family = parsed.command === "help" ? parsed.args[0]
      : COMMAND_REGISTRY.families.some((entry) => entry.name === parsed.command) ? parsed.command : undefined;
    write(parsed.flags.json ? `${JSON.stringify(COMMAND_REGISTRY)}\n` : commandHelp(family));
    return true;
  }
  return false;
}
