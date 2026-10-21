// Signing the cloud machine's Claude Code and Codex in from the phone.
//
// Every login is the CLI's own: `claude auth login`, `codex login`, or
// `codex login --device-auth`, run in a pty. Nothing here sees a password or a
// token; the CLI does the exchange and stores its own login file.
//
// Both browser logins open a URL whose redirect is a server the CLI runs on
// this machine (`http://localhost:<port>/callback` for Claude, port 1455 for
// Codex). BROWSER points at a script that records that URL instead of opening
// it, and the phone opens it. The iOS app catches the redirect on the phone's
// own localhost and hands the query back; finish() replays it against the
// CLI's server here, which is the callback the CLI was waiting for.
//
// Without the iOS app there is no way to catch a localhost redirect, so:
//   - Claude: the same run also prints a URL whose page shows a code, and
//     "Paste code here" takes it (finish with `code`). Both URLs carry the same
//     state, so either way completes the one login.
//   - Codex: a device-code login (link + one-time code) that finishes on its
//     own; the app watches the status.

import fs from "node:fs";
import path from "node:path";
import { randomUUID } from "node:crypto";

// Long enough for a sign-in that needs a password reset; the codes expire
// before this anyway (Codex says 15 minutes).
const LOGIN_TTL_MS = 15 * 60 * 1000;
// A finished login stays in the snapshot so the app can say how it ended.
const ENDED_TTL_MS = 10 * 60 * 1000;
const URL_WAIT_MS = 45_000;
const EXIT_WAIT_MS = 60_000;

const ANSI = /\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b[@-_]/g;

export const LOGIN_PROVIDERS = ["claude", "codex"];

export function stripAnsi(text) {
  return String(text).replace(ANSI, "").replace(/\r/g, "");
}

// The redirect target of an authorize URL, when it is a server on this
// machine: what the phone listens on and what finish() replays against.
export function localCallback(authorizeUrl) {
  let url;
  try {
    url = new URL(authorizeUrl);
  } catch {
    return null;
  }
  const state = url.searchParams.get("state");
  const redirect = url.searchParams.get("redirect_uri");
  if (!state || !redirect) {
    return null;
  }
  let target;
  try {
    target = new URL(redirect);
  } catch {
    return null;
  }
  if (target.protocol !== "http:" || !["localhost", "127.0.0.1", "[::1]"].includes(target.hostname)) {
    return null;
  }
  const port = Number(target.port);
  if (!Number.isInteger(port) || port <= 0 || port > 65535) {
    return null;
  }
  return { host: target.hostname, port, path: target.pathname || "/", state };
}

// Claude's "If the browser didn't open, visit: <url>": the authorize URL whose
// page shows a code to paste (its redirect is Anthropic's, not localhost).
export function claudeCodeUrl(output) {
  for (const match of stripAnsi(output).matchAll(/https:\/\/[^\s"'<>]+\/oauth\/authorize\?[^\s"'<>]+/g)) {
    if (!localCallback(match[0])) {
      return match[0];
    }
  }
  return null;
}

// Codex's device login: "Open this link ... https://auth.openai.com/codex/device"
// then "Enter this one-time code (expires in 15 minutes)  ABCD-EFGHI".
export function codexDeviceCode(output) {
  const text = stripAnsi(output);
  const link = /https:\/\/auth\.openai\.com\/[^\s"'<>]*device[^\s"'<>]*/.exec(text)?.[0];
  const code = /one-time code[^\n]*\n\s*([A-Z0-9]{3,}(?:-[A-Z0-9]{3,})+)/i.exec(text)?.[1];
  return link && code ? { verifyUrl: link, userCode: code } : null;
}

// The CLI's last words, for an error: its own prompts and the URLs are noise.
export function lastLines(output, count = 3) {
  return stripAnsi(output)
    .split("\n")
    .map((line) => line.replace(/^.*paste code here if prompted >\s*/i, "").replace(/https?:\/\/\S+/g, "").trim())
    .filter((line) => line && !/^(opening browser|if the browser didn.t open|if your browser did not open|starting local login server|on a remote or headless|warning: proceeding)/i.test(line))
    .slice(-count)
    .join(" ")
    .slice(0, 400);
}

/**
 * @param {{
 *   spawnPty: (file: string, args: string[], options: object) => any,
 *   env: Record<string, string | undefined>,
 *   dir: string,
 *   cwd: string,
 *   log?: (message: string) => void,
 *   onChange?: (provider: string, session: object) => void,
 *   fetchImpl?: typeof fetch,
 *   commands?: Partial<Record<string, string>>,
 * }} options
 */
export function createLoginRunner({ spawnPty, env, dir, cwd, log = () => {}, onChange = () => {}, fetchImpl = fetch, commands = {} }) {
  const sessions = new Map(); // provider -> session
  fs.mkdirSync(dir, { recursive: true, mode: 0o700 });
  const browserScript = path.join(dir, "browser.sh");
  fs.writeFileSync(
    browserScript,
    "#!/bin/sh\n# Records the sign-in URL a CLI would open; the phone opens it.\nprintf '%s\\n' \"$1\" >> \"$RUDDER_LOGIN_URL_FILE\"\n",
    { mode: 0o700 },
  );
  fs.chmodSync(browserScript, 0o700);

  function changed(session) {
    try {
      onChange(session.provider, publicView(session));
    } catch (error) {
      log(`login ${session.provider}: ${error.message}`);
    }
  }

  function end(session, status, error = null) {
    if (session.status === "done" || session.status === "failed" || session.status === "cancelled") {
      return;
    }
    session.status = status;
    session.error = error;
    session.endedAt = new Date().toISOString();
    clearTimeout(session.ttl);
    try { fs.rmSync(session.urlFile, { force: true }); } catch { /* ignore */ }
    if (status !== "done") {
      try { session.term?.kill(); } catch { /* already gone */ }
    }
    log(`login ${session.provider}: ${status}${error ? ` (${error})` : ""}`);
    for (const waiter of session.waiters.splice(0)) {
      waiter();
    }
    changed(session);
  }

  function waitForEnd(session, timeoutMs) {
    if (session.endedAt) {
      return Promise.resolve();
    }
    return new Promise((resolve) => {
      const timer = setTimeout(resolve, timeoutMs);
      session.waiters.push(() => {
        clearTimeout(timer);
        resolve();
      });
    });
  }

  async function start(provider, { device = false } = {}) {
    if (!LOGIN_PROVIDERS.includes(provider)) {
      throw new Error(`unknown provider ${provider}`);
    }
    const previous = sessions.get(provider);
    if (previous && !previous.endedAt) {
      end(previous, "cancelled", "replaced by a new sign-in");
    }
    const id = randomUUID();
    const mode = provider === "codex" && device ? "device" : "browser";
    const urlFile = path.join(dir, `${id}.url`);
    const session = {
      id,
      provider,
      mode,
      status: "starting",
      error: null,
      startedAt: new Date().toISOString(),
      endedAt: null,
      urlFile,
      output: "",
      url: null,
      callback: null,
      codeUrl: null,
      userCode: null,
      verifyUrl: null,
      term: null,
      exitCode: null,
      waiters: [],
      ttl: null,
    };
    sessions.set(provider, session);

    const file = commands[provider] || provider;
    const args = provider === "claude"
      ? ["auth", "login", "--claudeai"]
      : mode === "device" ? ["login", "--device-auth"] : ["login"];
    const childEnv = { ...env, BROWSER: browserScript, RUDDER_LOGIN_URL_FILE: urlFile, TERM: "xterm-256color" };
    // An API key in the environment would decide the login for the CLI.
    delete childEnv.ANTHROPIC_API_KEY;
    delete childEnv.CLAUDE_CODE_OAUTH_TOKEN;
    let term;
    try {
      // Wide, so a URL is never wrapped onto two lines by the CLI.
      term = spawnPty(file, args, { name: "xterm-256color", cols: 2000, rows: 50, cwd, env: childEnv });
    } catch (error) {
      end(session, "failed", `could not start ${file}: ${error.message}`);
      throw new Error(session.error);
    }
    session.term = term;
    term.onData((data) => {
      session.output = (session.output + data).slice(-64_000);
    });
    term.onExit(({ exitCode }) => {
      session.exitCode = exitCode;
      // The last line (the reason it failed) can land just after the exit.
      setTimeout(() => {
        if (exitCode === 0 && session.status !== "starting") {
          end(session, "done");
        } else {
          end(session, "failed", lastLines(session.output) || `${file} exited with code ${exitCode}`);
        }
      }, 120);
    });
    session.ttl = setTimeout(() => end(session, "failed", "The sign-in took too long. Start it again."), LOGIN_TTL_MS);
    session.ttl.unref?.();

    const deadline = Date.now() + URL_WAIT_MS;
    while (!session.endedAt && Date.now() < deadline) {
      if (readUrls(session)) {
        session.status = "waiting";
        changed(session);
        return publicStart(session);
      }
      await sleep(150);
    }
    if (!session.endedAt) {
      end(session, "failed", `${provider === "claude" ? "Claude Code" : "Codex"} did not show a sign-in link (${lastLines(session.output) || "no output"})`);
    }
    throw new Error(session.error || "sign-in failed to start");
  }

  // True once everything the phone needs for this login has appeared.
  function readUrls(session) {
    if (!session.url) {
      let opened = "";
      try {
        opened = fs.readFileSync(session.urlFile, "utf8");
      } catch {
        // not yet
      }
      // A CLI that ignores $BROWSER (Codex on macOS) still prints the link:
      // take the printed authorize URL whose redirect comes back to this machine.
      const printed = [...stripAnsi(session.output).matchAll(/https:\/\/[^\s"'<>]+/g)].map((m) => m[0]).find((u) => localCallback(u));
      const first = opened.split("\n").map((line) => line.trim()).find((line) => /^https:\/\//.test(line)) || printed;
      if (first) {
        session.url = first;
        session.callback = localCallback(first);
      }
    }
    if (session.provider === "claude") {
      session.codeUrl ||= claudeCodeUrl(session.output);
      return Boolean(session.url && session.callback && session.codeUrl);
    }
    if (session.mode === "device") {
      const device = codexDeviceCode(session.output);
      if (device) {
        session.userCode = device.userCode;
        session.verifyUrl = device.verifyUrl;
      }
      return Boolean(device);
    }
    return Boolean(session.url && session.callback);
  }

  async function finish(provider, id, { callback, code } = {}) {
    const session = sessions.get(provider);
    if (!session || session.id !== id) {
      throw new Error("That sign-in is no longer running. Start it again.");
    }
    if (session.endedAt) {
      return publicView(session);
    }
    if (session.status !== "waiting") {
      throw new Error("That sign-in is already finishing.");
    }
    if (typeof callback === "string" && callback) {
      if (!session.callback) {
        throw new Error("This sign-in has no callback to finish.");
      }
      const query = new URLSearchParams(callback.replace(/^[^?]*\?/, ""));
      // A callback with the wrong state ends Claude's login outright, so a
      // stray or stale one never reaches the CLI.
      if (query.get("state") !== session.callback.state) {
        throw new Error("That sign-in link belongs to an older attempt. Start it again.");
      }
      if (!query.get("code")) {
        const reason = query.get("error_description") || query.get("error") || "no code came back";
        end(session, "failed", `Sign-in was not approved (${reason})`);
        return publicView(session);
      }
      session.status = "finishing";
      changed(session);
      const host = session.callback.host === "[::1]" ? "[::1]" : "127.0.0.1";
      try {
        await fetchImpl(`http://${host}:${session.callback.port}${session.callback.path}?${query.toString()}`, {
          redirect: "manual",
          signal: AbortSignal.timeout(30_000),
        });
      } catch (error) {
        // The CLI may have closed the connection as it exited; its exit code
        // is the answer either way.
        log(`login ${provider}: callback request: ${error.message}`);
      }
    } else if (typeof code === "string" && code.trim()) {
      if (provider !== "claude") {
        throw new Error("Codex signs in with its device code, not a pasted one.");
      }
      const cleaned = code.trim().replace(/\s+/g, "");
      if (!/^[A-Za-z0-9._~#=-]+$/.test(cleaned) || cleaned.length > 4000) {
        throw new Error("That doesn't look like the code from the sign-in page.");
      }
      session.status = "finishing";
      changed(session);
      session.term.write(`${cleaned}\r`);
    } else {
      throw new Error("callback or code is required");
    }
    await waitForEnd(session, EXIT_WAIT_MS);
    if (!session.endedAt) {
      end(session, "failed", lastLines(session.output) || "The sign-in did not finish.");
    }
    return publicView(session);
  }

  function cancel(provider, id) {
    const session = sessions.get(provider);
    if (session && (!id || session.id === id) && !session.endedAt) {
      end(session, "cancelled");
    }
    return session ? publicView(session) : null;
  }

  // What the control plane keeps and the app shows. URLs and codes are
  // handed to the one request that started the sign-in, not broadcast.
  function snapshot() {
    const out = {};
    const now = Date.now();
    for (const [provider, session] of sessions) {
      if (session.endedAt && now - Date.parse(session.endedAt) > ENDED_TTL_MS) {
        continue;
      }
      out[provider] = publicView(session);
    }
    return out;
  }

  function dispose() {
    for (const session of sessions.values()) {
      if (!session.endedAt) {
        end(session, "cancelled");
      }
    }
  }

  return { start, finish, cancel, snapshot, dispose };
}

function publicView(session) {
  return {
    id: session.id,
    provider: session.provider,
    mode: session.mode,
    status: session.status,
    error: session.error,
    startedAt: session.startedAt,
    endedAt: session.endedAt,
  };
}

function publicStart(session) {
  return {
    ...publicView(session),
    url: session.mode === "device" ? session.verifyUrl : session.url,
    callback: session.callback ? { port: session.callback.port, path: session.callback.path } : null,
    codeUrl: session.codeUrl,
    userCode: session.userCode,
  };
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
