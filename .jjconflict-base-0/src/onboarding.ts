// A new computer, right after `rudder login` (and from `rudder doctor`): can it
// run agents, and what is already waiting in the cloud? Claude Code and Codex
// are offered for install and their own sign-in, at a y/N prompt, only on an
// interactive terminal. Their logins are never copied from the cloud vault:
// both rotate refresh tokens, and a shared copy signs one side out.
import { spawnSync } from "node:child_process";
import readline from "node:readline/promises";
import { commandEnv, commandExists } from "./util.js";

type AgentCli = {
  command: "claude" | "codex";
  label: string;
  install: string[];
  login: string[];
  signedIn: () => boolean;
};

const AGENT_CLIS: AgentCli[] = [
  {
    command: "claude",
    label: "Claude Code",
    install: ["npm", "i", "-g", "@anthropic-ai/claude-code"],
    login: ["claude", "auth", "login"],
    signedIn: () => {
      const status = spawnSync("claude", ["auth", "status", "--json"], { encoding: "utf8", timeout: 20_000, env: commandEnv() });
      try {
        return Boolean(JSON.parse(status.stdout).loggedIn);
      } catch {
        return false;
      }
    },
  },
  {
    command: "codex",
    label: "Codex",
    install: ["npm", "i", "-g", "@openai/codex"],
    login: ["codex", "login"],
    signedIn: () => spawnSync("codex", ["login", "status"], { stdio: "ignore", timeout: 20_000, env: commandEnv() }).status === 0,
  },
];

export type CloudRepoSummary = { repo: string; agents: number };

export type OnboardingOptions = {
  json?: boolean;
  /** The account's repos on its cloud machine; null when not signed in. */
  cloudRepos?: () => Promise<CloudRepoSummary[] | null>;
};

export function onboardingInteractive(json?: boolean): boolean {
  return !json && Boolean(process.stdin.isTTY && process.stdout.isTTY) && process.env.RUDDER_SKIP_ONBOARDING !== "1";
}

async function confirm(question: string): Promise<boolean> {
  const rl = readline.createInterface({ input: process.stdin, output: process.stdout });
  try {
    return /^y(es)?$/i.test((await rl.question(`${question} [y/N] `)).trim());
  } finally {
    rl.close();
  }
}

function run(argv: string[]): boolean {
  console.log(`$ ${argv.join(" ")}`);
  return spawnSync(argv[0], argv.slice(1), { stdio: "inherit", env: commandEnv() }).status === 0;
}

async function offerInstall(cli: AgentCli): Promise<boolean> {
  if (!(await confirm(`Install ${cli.label} now (${cli.install.join(" ")})?`))) {
    return false;
  }
  if (!run(cli.install) || !commandExists(cli.command)) {
    console.log(`${cli.label} did not install. Try \`${cli.install.join(" ")}\` yourself (it may need sudo, or a Node version manager).`);
    return false;
  }
  return true;
}

async function offerLogin(cli: AgentCli): Promise<void> {
  if (await confirm(`Sign in to ${cli.label} now (${cli.login.join(" ")})?`)) {
    run(cli.login);
  } else {
    console.log(`Later: \`${cli.login.join(" ")}\`.`);
  }
}

export async function runLaptopOnboarding(options: OnboardingOptions = {}): Promise<void> {
  if (!onboardingInteractive(options.json)) {
    return;
  }
  console.log("");
  const installed = AGENT_CLIS.filter((cli) => commandExists(cli.command));
  if (installed.length === 0) {
    console.log("Rudder runs agents through Claude Code or Codex, and neither is installed here.");
    for (const cli of AGENT_CLIS) {
      if (await offerInstall(cli)) {
        await offerLogin(cli);
      }
    }
  } else {
    for (const cli of installed) {
      if (cli.signedIn()) {
        console.log(`${cli.label}: signed in.`);
      } else {
        console.log(`${cli.label}: installed, not signed in on this computer.`);
        await offerLogin(cli);
      }
    }
  }
  if (!AGENT_CLIS.some((cli) => commandExists(cli.command))) {
    console.log("Install one later with `npm i -g @anthropic-ai/claude-code` or `npm i -g @openai/codex`.");
  }

  const repos = options.cloudRepos ? await options.cloudRepos().catch(() => null) : null;
  if (!repos) {
    return;
  }
  console.log("");
  if (repos.length === 0) {
    console.log("Nothing on your cloud machine yet. Run `rudder cloud` in a repo here, or add one from the phone app.");
    return;
  }
  console.log(`On your cloud machine (${repos.length} ${repos.length === 1 ? "repo" : "repos"}):`);
  for (const repo of repos) {
    console.log(`  ${repo.repo}${repo.agents ? `  ${repo.agents} ${repo.agents === 1 ? "agent" : "agents"}` : ""}`);
  }
  const example = repos[0].repo.split("/").pop();
  console.log(`\`rudder cloud sync ${example}\` clones one into ~/code with its work.`);
}
