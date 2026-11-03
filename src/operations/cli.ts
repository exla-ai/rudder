import { assertAliasCurrent, COMMAND_REGISTRY, commandCompletions, commandHelp } from "./registry.js";
import { deterministicTaskRoute } from "../task-routing.js";

type ParsedSurface = { command?: string; args: string[]; flags: { help?: boolean; json?: boolean; backend?: string; model?: string; workspace?: boolean } };

/** Normalize canonical families before existing dispatch. Never wrap its stdout,
 * errors or process exit codes. Required callbacks pass through byte for byte. */
export function applyCommandSurface<T extends ParsedSurface>(input: T, now = new Date(), rawArgv?: readonly string[]): T {
  const parsed = { ...input, args: [...input.args], flags: { ...input.flags } };
  const name = parsed.command;
  if (!name && parsed.args.length && !parsed.flags.help && !("version" in parsed.flags && parsed.flags.version)) {
    return applyCommandSurface({ ...parsed, command: "run" }, now, rawArgv);
  }
  if (!name || name.startsWith("__") || COMMAND_REGISTRY.callbacks.includes(name)
      || ["help", "completion", "version"].includes(name) || parsed.flags.help) return parsed;
  const delimiter = rawArgv?.indexOf("--") ?? -1;
  const flagArgv = delimiter >= 0 ? rawArgv!.slice(0, delimiter) : rawArgv ?? [];
  for (const alias of COMMAND_REGISTRY.flag_aliases) if (flagArgv.includes(alias.name)) assertAliasCurrent(alias, now);
  if (COMMAND_REGISTRY.retired.includes(name)) throw new Error(`${name} is retired; use rudder open`);
  const family = COMMAND_REGISTRY.families.find((entry) => entry.name === name);
  let operation;
  if (family) {
    const bareAlias = COMMAND_REGISTRY.aliases.find((entry) => entry.name === name && "bare_only" in entry && entry.bare_only);
    if (bareAlias && parsed.args.length === 0) {
      assertAliasCurrent(bareAlias, now); parsed.command = bareAlias.adapter ?? undefined; return parsed;
    }
    const candidates = COMMAND_REGISTRY.operations.filter((entry) => entry.cli[0] === name);
    operation = candidates.find((entry) => entry.cli.length === 2 && entry.cli[1] === parsed.args[0]);
    if (operation) parsed.args.shift();
    else operation = candidates.find((entry) => entry.cli.length === 1);
    if (!operation) throw new Error(`Choose a ${name} action: ${commandCompletions([name]).join(", ")}`);
    if (name === "doctor" && parsed.args.length && operation.cli.length === 1) {
      throw new Error(`Unknown doctor action: ${parsed.args[0]}`);
    }
    if (!operation.adapter) throw new Error(`${operation.id} requires a selected surface or authenticated engine tool; no task was started`);
    parsed.command = operation.adapter;
  } else {
    const alias = COMMAND_REGISTRY.aliases.find((entry) => entry.name === name);
    if (alias) {
      assertAliasCurrent(alias, now);
      parsed.command = alias.adapter ?? undefined;
      if ("backend" in alias && alias.backend) {
        if (parsed.args.some((arg) => /^--(?:location|checkout)(?:=|$)/.test(arg))) throw new Error(`Use ${alias.replacement} to select an execution scope`);
        if (parsed.flags.backend && parsed.flags.backend !== alias.backend) throw new Error("Conflicting backend selection");
        parsed.flags.backend = alias.backend;
        if (alias.backend === "acpx" && parsed.args[0] === "codex") parsed.args.shift();
      }
      // Compatibility starts retain their old checkout policy. New canonical
      // starts and ordinary text choose the documented isolated default below.
      return parsed;
    }
    if (name.startsWith("/") || name.startsWith("-")) throw new Error(`Unknown Rudder command: ${name}`);
    parsed.command = "run";
    parsed.args.unshift(name);
  }
  if (parsed.command === "dashboard" && parsed.args.length) parsed.command = "attach";
  if (parsed.command === "run") {
    let location = "local";
    let checkout = "isolated";
    // These are surface flags, intentionally consumed before legacy dispatch.
    const task: string[] = [];
    const literalCount = delimiter >= 0 ? rawArgv!.length - delimiter - 1 : 0;
    const flagEnd = parsed.args.length - literalCount;
    for (let i = 0; i < parsed.args.length; i++) {
      const arg = parsed.args[i]!;
      if (i >= flagEnd) { task.push(arg); continue; }
      if (arg === "--location" || arg === "--checkout") {
        if (i + 1 >= flagEnd) throw new Error(`${arg} requires a value`);
        const value = parsed.args[++i];
        if (!value) throw new Error(`${arg} requires a value`);
        if (arg === "--location") location = value;
        else checkout = value;
      } else if (arg.startsWith("--location=")) location = arg.slice(11);
      else if (arg.startsWith("--checkout=")) checkout = arg.slice(11);
      else task.push(arg);
    }
    if (!["local", "cloud"].includes(location) || !["isolated", "main"].includes(checkout)) throw new Error("Choose --location local|cloud and --checkout isolated|main");
    if (checkout === "main") throw new Error("Main checkout execution requires the native task owner; choose that checkout in rudder open");
    if (!task.join(" ").trim()) throw new Error('Missing task. Usage: rudder run "fix the tests"');
    const route = deterministicTaskRoute(task.join(" "));
    if (route.route !== "execute") throw new Error(`${route.reason}; use rudder open to preserve the native planning and approval owner`);
    if (location === "cloud" && (parsed.flags.backend || parsed.flags.model)) {
      throw new Error("Cloud task launch cannot preserve an explicit backend/model through this adapter; select them in rudder open");
    }
    parsed.flags.workspace = true;
    // The sail dispatch is task-only. Bare cloud text also guesses project
    // names, which would turn a one-word task into an attach operation.
    parsed.command = location === "cloud" ? "sail" : "run";
    parsed.args = task;
  }
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
