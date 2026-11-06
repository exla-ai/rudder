// `rudder cloud host <ssh-target>`: run the Rudder Cloud home on any Linux host
// with Docker (a bare-metal box, a cloud VM) instead of a Fly machine. The
// host side is cloud/host/ (setup script, start and update scripts, systemd
// units); the control plane side is cloud/src/home-host.ts.
//
//   1. copy cloud/host to the host and run rudder-host-setup (Docker, rsync,
//      sysctls, the data disk at /srv/workspace, the units)
//   2. check the host can pull the current worker image (the public
//      ghcr.io/exla-ai/rudder-worker copy, so no registry login)
//   3. POST /api/rudder/home/host {kind:"external"}: the control plane rotates
//      the worker token (the Fly machine's stops working) and returns the env
//      bundle once
//   4. write it to /etc/rudder/home.env (0600) over ssh stdin, never argv, and
//      the image to /etc/rudder/image, then start rudder-home.service
//
// Moving the disk is the operator's step (--copy-from-fly prints it): the home
// on the new host starts from whatever /srv/workspace holds.

import { spawn } from "node:child_process";
import { randomBytes } from "node:crypto";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

export type HostArgs = {
  target: string;
  dataDisk?: string;
  hostname?: string;
  copyFromFly: boolean;
  skipSetup: boolean;
  yes: boolean;
};

export type HostClient = {
  request<T>(pathOrUrl: string, init: { method: string; body?: unknown }): Promise<T>;
};

type HostHome = { id: string; status: string; connected: boolean; hostKind?: string; hostImage?: string | null } | null;

const USAGE = "Usage: rudder cloud host <ssh-target> [--data-disk /dev/X] [--hostname <name>] [--skip-setup] [--copy-from-fly] [--yes]";
// The files rudder-host-setup installs; it must sit beside them.
export const HOST_FILES = [
  "rudder-host-setup",
  "rudder-home-start",
  "rudder-home-apply-update",
  "rudder-home.service",
  "rudder-home-update.service",
  "rudder-home-update.path",
  "90-rudder.conf",
];

// The same shape cloud/worker/home-host.mjs and rudder-home-apply-update accept.
export function validImage(value: unknown): value is string {
  return typeof value === "string" && /^[A-Za-z0-9][A-Za-z0-9._/:@-]{0,255}$/.test(value);
}

export function parseHostArgs(args: string[], yes = false): HostArgs {
  const parsed: HostArgs = { target: "", copyFromFly: false, skipSetup: false, yes };
  for (let i = 0; i < args.length; i += 1) {
    const arg = args[i] ?? "";
    const [flag, inline] = arg.startsWith("--") && arg.includes("=") ? [arg.slice(0, arg.indexOf("=")), arg.slice(arg.indexOf("=") + 1)] : [arg, undefined];
    const value = () => {
      const next = inline ?? args[++i];
      if (!next || next.startsWith("-")) throw new Error(`${flag} needs a value. ${USAGE}`);
      return next;
    };
    if (flag === "--data-disk") parsed.dataDisk = value();
    else if (flag === "--hostname") parsed.hostname = value();
    else if (flag === "--copy-from-fly") parsed.copyFromFly = true;
    else if (flag === "--skip-setup") parsed.skipSetup = true;
    else if (flag === "--yes" || flag === "-y") parsed.yes = true;
    else if (!arg.startsWith("-") && !parsed.target) parsed.target = arg;
    else throw new Error(`unexpected argument ${arg}. ${USAGE}`);
  }
  // Arguments reach ssh as argv, never through a shell, but a target that
  // looks like an option would still be read as one.
  if (!/^[A-Za-z0-9._@:\[\]-]+$/.test(parsed.target) || parsed.target.startsWith("-")) {
    throw new Error(USAGE);
  }
  if (parsed.dataDisk !== undefined && !/^\/dev\/[A-Za-z0-9/_-]+$/.test(parsed.dataDisk)) {
    throw new Error("--data-disk must be a device path like /dev/nvme1n1");
  }
  if (parsed.hostname !== undefined && !/^[A-Za-z0-9][A-Za-z0-9-]{0,62}$/.test(parsed.hostname)) {
    throw new Error("--hostname must be a single DNS label");
  }
  return parsed;
}

/** The env bundle as a Docker --env-file. Refuses anything that would not round-trip. */
export function renderHomeEnv(env: Record<string, unknown>): string {
  const lines: string[] = [];
  for (const [key, value] of Object.entries(env)) {
    if (!/^[A-Z][A-Z0-9_]*$/.test(key) || typeof value !== "string" || /[\r\n\0]/.test(value)) {
      throw new Error(`the control plane returned an env entry that cannot go in an env file (${key})`);
    }
    lines.push(`${key}=${value}`);
  }
  if (!lines.some((line) => line.startsWith("RUDDER_WORKER_TOKEN="))) {
    throw new Error("the control plane did not return a worker token");
  }
  return `${lines.join("\n")}\n`;
}

/** The manual disk copy from the Fly home, printed by --copy-from-fly. */
export function copyFromFlySteps(target: string, filesDir = hostFilesDir()): string {
  return `Copy the Fly home's disk to ${target} before switching; the home keeps running on Fly until step 4.

  1. Prepare the host without switching it over:
       scp -r ${filesDir} ${target}:rudder-host
       ssh ${target} sudo rudder-host/rudder-host-setup [--data-disk /dev/<disk>]
  2. Bulk copy while agents keep working (repeat as often as you like). In
     \`fly ssh console\` on the home machine: apt-get install -y rsync, add a key
     ${target} accepts, then
       rsync -aHAX --numeric-ids --info=progress2 --exclude=/.rudder-swap \\
         --rsync-path="sudo rsync" /workspace/ ${target}:/srv/workspace/
  3. Freeze it: stop the home (rudder cloud home stop), start the Fly machine with
     /bin/sleep infinity as its init so no agent writes, and run the rsync once
     more with --delete.
  4. Switch: rudder cloud host ${target} --skip-setup
     The Fly machine and volume are left as they are; destroy them once the new
     host checks out.`;
}

export function hostFilesDir(): string {
  return path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "cloud", "host");
}

type RunResult = { code: number; stdout: string };

function run(command: string, args: string[], options: { input?: string; inherit?: boolean } = {}): Promise<RunResult> {
  return new Promise((resolve, reject) => {
    const child = spawn(command, args, {
      stdio: [options.input === undefined ? (options.inherit ? "inherit" : "ignore") : "pipe", options.inherit ? "inherit" : "pipe", "inherit"],
    });
    let stdout = "";
    child.stdout?.on("data", (chunk) => { stdout += String(chunk); });
    child.on("error", reject);
    child.on("close", (code) => resolve({ code: code ?? 1, stdout }));
    if (options.input !== undefined) {
      child.stdin?.end(options.input);
    }
  });
}

async function ssh(target: string, remote: string[], options: { input?: string; inherit?: boolean } = {}): Promise<RunResult> {
  return await run("ssh", ["-o", "BatchMode=yes", target, ...remote], options);
}

async function must(step: string, result: Promise<RunResult>): Promise<RunResult> {
  const done = await result;
  if (done.code !== 0) {
    throw new Error(`${step} failed (exit ${done.code})`);
  }
  return done;
}

async function confirmSwitch(target: string, yes: boolean): Promise<boolean> {
  if (yes) {
    return true;
  }
  if (!process.stdin.isTTY || !process.stdout.isTTY) {
    throw new Error("Refusing to switch the home without confirmation; pass --yes to confirm.");
  }
  const readline = await import("node:readline/promises");
  const rl = readline.createInterface({ input: process.stdin, output: process.stdout });
  try {
    const answer = await rl.question(`Run your home on ${target}? Its Fly machine stops being able to connect, and the home starts from what ${target}:/srv/workspace holds. [y/N] `);
    return /^y(es)?$/i.test(answer.trim());
  } finally {
    rl.close();
  }
}

export async function hostCommand(rawArgs: string[], client: HostClient, options: { json?: boolean; yes?: boolean }): Promise<void> {
  const args = parseHostArgs(rawArgs, Boolean(options.yes));
  if (args.copyFromFly) {
    console.log(copyFromFlySteps(args.target));
    return;
  }
  const current = await client.request<{ hostKind: string | null; image: string; home: HostHome }>("/api/rudder/home/host", { method: "GET" });
  if (!current.home) {
    throw new Error("you have no cloud home yet; create one with `rudder cloud` first");
  }
  // ssh joins its remote argv into one shell line, so every value placed there
  // has a shape with nothing a shell would read.
  if (!validImage(current.image)) {
    throw new Error("the control plane returned an image reference this command will not pass to a shell");
  }
  await must(`ssh ${args.target}`, ssh(args.target, ["sudo", "-n", "true"]));

  if (!args.skipSetup) {
    const dir = hostFilesDir();
    const files = HOST_FILES.map((name) => path.join(dir, name));
    for (const file of files) {
      if (!fs.existsSync(file)) throw new Error(`missing ${file}; reinstall rudder`);
    }
    const remoteDir = `rudder-host-${randomBytes(4).toString("hex")}`;
    console.log(`Setting up ${args.target} (Docker, sysctls, /srv/workspace, systemd units)...`);
    await must("mkdir on the host", ssh(args.target, ["mkdir", "-p", remoteDir]));
    await must("copying cloud/host", run("scp", ["-q", "-o", "BatchMode=yes", ...files, `${args.target}:${remoteDir}/`]));
    await must("rudder-host-setup", ssh(args.target, ["sudo", "-n", `./${remoteDir}/rudder-host-setup`, ...(args.dataDisk ? ["--data-disk", args.dataDisk] : [])], { inherit: true }));
    await ssh(args.target, ["rm", "-rf", remoteDir]);
  }

  // Before anything changes: a host that cannot pull the image would leave
  // the home switched over with nothing able to run it.
  console.log(`Pulling ${current.image} on ${args.target}...`);
  const pulled = await ssh(args.target, ["sudo", "-n", "docker", "pull", "-q", current.image]);
  if (pulled.code !== 0) {
    const registry = current.image.includes("/") ? current.image.slice(0, current.image.indexOf("/")) : "docker.io";
    throw new Error(`${args.target} cannot pull ${current.image}. Check it can reach ${registry} (a private registry also needs \`sudo docker login ${registry}\` on the host), then run this again.`);
  }

  if (!(await confirmSwitch(args.target, args.yes))) {
    console.log("Nothing changed.");
    return;
  }
  const switched = await client.request<{ image: string; env: Record<string, unknown>; home: HostHome }>("/api/rudder/home/host", {
    method: "POST",
    body: { kind: "external" },
  });
  const envFile = renderHomeEnv(switched.env);
  const recover = `The home is switched but ${args.target} is not running it yet; run \`rudder cloud host ${args.target} --skip-setup\` again.`;
  try {
    // stdin, never argv: the token must not show up in a process list.
    await must("writing /etc/rudder/home.env", ssh(args.target, ["sudo", "-n", "install", "-m", "600", "-o", "root", "-g", "root", "/dev/stdin", "/etc/rudder/home.env"], { input: envFile }));
    await must("writing /etc/rudder/image", ssh(args.target, ["sudo", "-n", "install", "-m", "644", "/dev/stdin", "/etc/rudder/image"], { input: `${switched.image}\n` }));
    if (args.hostname) {
      await must("writing /etc/rudder/hostname", ssh(args.target, ["sudo", "-n", "install", "-m", "644", "/dev/stdin", "/etc/rudder/hostname"], { input: `${args.hostname}\n` }));
    }
    await must("starting rudder-home", ssh(args.target, ["sudo", "-n", "systemctl", "restart", "rudder-home.service"], { inherit: true }));
  } catch (error) {
    throw new Error(`${error instanceof Error ? error.message : String(error)}. ${recover}`);
  }

  console.log(`Waiting for the home on ${args.target} to connect...`);
  for (let i = 0; i < 60; i += 1) {
    await new Promise((resolve) => setTimeout(resolve, 3000));
    const { home } = await client.request<{ home: HostHome }>("/api/rudder/home", { method: "GET" }).catch(() => ({ home: null }));
    if (home?.connected) {
      if (options.json) {
        console.log(JSON.stringify({ home, hostKind: "external", image: switched.image, target: args.target }, null, 2));
      } else {
        console.log(`Your home runs on ${args.target} now (${switched.image}). Updates arrive there when it is idle.`);
        console.log("The Fly machine and volume were left as they are; destroy them once everything checks out.");
      }
      return;
    }
  }
  throw new Error(`the home did not connect within 3 minutes; check \`sudo systemctl status rudder-home\` and \`sudo docker logs rudder-home\` on ${args.target}`);
}
