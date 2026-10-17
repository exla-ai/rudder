// Signing the cloud machine's Claude Code and Codex in from the phone
// (cloud/worker/logins.mjs), against fake CLIs that behave like the real ones:
// they "open" an authorize URL through $BROWSER, wait on a localhost callback
// server, and Claude also takes a pasted code. The real flows were checked by
// hand against claude 2.1.302 and codex 0.163 (URL shapes, the 400 a bad state
// gets, Claude ending its login on one).
import test from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

import { createLoginRunner, localCallback, claudeCodeUrl, codexDeviceCode } from "../cloud/worker/logins.mjs";

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function tmpdir() {
  return fs.mkdtempSync(path.join(os.tmpdir(), "rudder-logins-"));
}

async function waitFor(fn, timeout = 8000) {
  const start = Date.now();
  for (;;) {
    const value = await fn();
    if (value) return value;
    if (Date.now() - start > timeout) throw new Error("waitFor timed out");
    await sleep(40);
  }
}

// node-pty's surface, over plain pipes: enough for the runner.
function pipePty(file, args, { env, cwd }) {
  const child = spawn(file, args, { env, cwd, stdio: ["pipe", "pipe", "pipe"] });
  return {
    pid: child.pid,
    onData(cb) { child.stdout.on("data", (d) => cb(d.toString())); child.stderr.on("data", (d) => cb(d.toString())); },
    onExit(cb) { child.on("exit", (code, signal) => cb({ exitCode: code ?? (signal ? 1 : 0) })); },
    write(data) { child.stdin.write(data); },
    kill() { child.kill("SIGTERM"); },
    alive: () => child.exitCode === null && child.signalCode === null,
  };
}

// One script plays both CLIs. Browser logins: start a callback server on a
// free port, hand the authorize URL to $BROWSER, print the paste-a-code URL
// (Claude), then exit 0 on a callback with the right state and a code, or 1
// on a wrong state, like Claude does. Device login: print Codex's steps and
// "complete" after FAKE_DEVICE_MS.
const FAKE = String.raw`#!/usr/bin/env node
const http = require("node:http");
const { execFileSync } = require("node:child_process");
const fs = require("node:fs");
const path = require("node:path");
const which = path.basename(process.argv[1]);
const args = process.argv.slice(2);
const state = "st_" + Math.random().toString(36).slice(2);
fs.writeFileSync(path.join(process.env.HOME, which + "-env.json"), JSON.stringify({ apiKey: process.env.ANTHROPIC_API_KEY || null, browser: process.env.BROWSER || null }));
const done = (code) => {
  if (code === 0) fs.writeFileSync(path.join(process.env.HOME, which + "-login.json"), JSON.stringify({ ok: true }));
  process.exit(code);
};
if (args.includes("--device-auth")) {
  console.log("\nFollow these steps to sign in with ChatGPT using device code authorization:\n");
  console.log("1. Open this link in your browser and sign in to your account\n   \u001b[94mhttps://auth.openai.com/codex/device\u001b[0m\n");
  console.log("2. Enter this one-time code \u001b[90m(expires in 15 minutes)\u001b[0m\n   \u001b[94mABCD-EFGHI\u001b[0m\n");
  setTimeout(() => done(0), Number(process.env.FAKE_DEVICE_MS || 800));
  setInterval(() => {}, 1000);
} else {
  const callbackPath = which === "codex" ? "/auth/callback" : "/callback";
  const server = http.createServer((req, res) => {
    const url = new URL(req.url, "http://localhost");
    if (url.pathname !== callbackPath) { res.writeHead(404); res.end(); return; }
    if (url.searchParams.get("state") !== state) {
      res.writeHead(400); res.end("bad state");
      if (which === "claude") { console.log("Login failed: Invalid state parameter"); setTimeout(() => done(1), 20); }
      return;
    }
    if (url.searchParams.get("code") === "reject") { res.writeHead(500); res.end(); console.log("Token exchange failed: invalid_grant"); setTimeout(() => done(1), 20); return; }
    res.writeHead(302, { location: "https://example.com/success" }); res.end();
    console.log("Login successful."); setTimeout(() => done(0), 20);
  });
  server.listen(0, "127.0.0.1", () => {
    const port = server.address().port;
    const host = which === "codex" ? "127.0.0.1" : "localhost";
    const redirect = encodeURIComponent("http://" + host + ":" + port + callbackPath);
    const authorize = "https://auth.example.com/oauth/authorize?client_id=x&response_type=code&redirect_uri=" + redirect + "&state=" + state;
    execFileSync(process.env.BROWSER, [authorize]);
    if (which === "claude") {
      const manual = "https://auth.example.com/oauth/authorize?code=true&client_id=x&redirect_uri=" + encodeURIComponent("https://platform.example.com/oauth/code/callback") + "&state=" + state;
      console.log("Opening browser to sign in…");
      console.log("If the browser didn't open, visit: \u001b]8;;" + manual + "\u0007" + manual + "\u001b]8;;\u0007");
      process.stdout.write("Paste code here if prompted > ");
      process.stdin.on("data", (d) => {
        const code = d.toString().trim();
        if (code === "good#" + state) { console.log("Login successful."); done(0); }
        else { console.log("OAuth error: Invalid code"); done(1); }
      });
    }
  });
}
`;

function setup(t) {
  const dir = tmpdir();
  const bin = path.join(dir, "bin");
  const home = path.join(dir, "home");
  fs.mkdirSync(bin);
  fs.mkdirSync(home);
  for (const name of ["claude", "codex"]) {
    fs.writeFileSync(path.join(bin, name), FAKE, { mode: 0o755 });
  }
  const terms = [];
  const changes = [];
  const runner = createLoginRunner({
    spawnPty: (file, args, options) => {
      const term = pipePty(file, args, options);
      terms.push(term);
      return term;
    },
    env: { ...process.env, PATH: `${bin}:${process.env.PATH}`, HOME: home, ANTHROPIC_API_KEY: "sk-should-not-leak" },
    dir: path.join(dir, "logins"),
    cwd: home,
    onChange: (provider, session) => changes.push(`${provider}:${session.status}`),
  });
  t.after(() => {
    runner.dispose();
    fs.rmSync(dir, { recursive: true, force: true });
  });
  return { runner, home, terms, changes };
}

test("parsing what the real CLIs print", () => {
  // claude 2.1.302: the URL handed to $BROWSER, and the one it prints.
  const opened = "https://claude.com/cai/oauth/authorize?code=true&client_id=9d1c&response_type=code&redirect_uri=http%3A%2F%2Flocalhost%3A41643%2Fcallback&scope=user%3Ainference&code_challenge=abc&code_challenge_method=S256&state=hDnk";
  assert.deepEqual(localCallback(opened), { host: "localhost", port: 41643, path: "/callback", state: "hDnk" });
  const printed = "Opening browser to sign in…\r\nIf the browser didn't open, visit: \u001b]8;;https://claude.com/cai/oauth/authorize?code=true&redirect_uri=https%3A%2F%2Fplatform.claude.com%2Foauth%2Fcode%2Fcallback&state=hDnk\u0007https://claude.com/cai/oauth/authorize?code=true&redirect_uri=https%3A%2F%2Fplatform.claude.com%2Foauth%2Fcode%2Fcallback&state=hDnk\u001b]8;;\u0007\r\nPaste code here if prompted >";
  assert.equal(claudeCodeUrl(printed), "https://claude.com/cai/oauth/authorize?code=true&redirect_uri=https%3A%2F%2Fplatform.claude.com%2Foauth%2Fcode%2Fcallback&state=hDnk");
  assert.equal(localCallback(claudeCodeUrl(printed)), null, "the paste-a-code page redirects to Anthropic, not here");
  // codex 0.163
  assert.deepEqual(
    localCallback("https://auth.openai.com/oauth/authorize?response_type=code&redirect_uri=http%3A%2F%2F127.0.0.1%3A1455%2Fauth%2Fcallback&state=Px"),
    { host: "127.0.0.1", port: 1455, path: "/auth/callback", state: "Px" },
  );
  const device = "1. Open this link in your browser and sign in to your account\r\n   \u001b[94mhttps://auth.openai.com/codex/device\u001b[0m\r\n\r\n2. Enter this one-time code \u001b[90m(expires in 15 minutes)\u001b[0m\r\n   \u001b[94m1QV4-VGXTW\u001b[0m\r\n";
  assert.deepEqual(codexDeviceCode(device), { verifyUrl: "https://auth.openai.com/codex/device", userCode: "1QV4-VGXTW" });
  // Anything that would send the callback somewhere else is not a local one.
  assert.equal(localCallback("https://x/authorize?redirect_uri=https%3A%2F%2Fevil.example%2Fcb&state=s"), null);
  assert.equal(localCallback("https://x/authorize?redirect_uri=http%3A%2F%2Flocalhost%3A99999%2Fcb&state=s"), null);
});

test("Claude: the phone's callback is replayed against the CLI, and a stale one never reaches it", async (t) => {
  const { runner, home, terms, changes } = setup(t);
  const started = await runner.start("claude");
  assert.equal(started.status, "waiting");
  assert.equal(started.callback.path, "/callback");
  assert.ok(started.callback.port > 0);
  assert.match(started.url, /redirect_uri=http%3A%2F%2Flocalhost/);
  assert.match(started.codeUrl, /code=true/);
  const state = new URL(started.url).searchParams.get("state");

  await assert.rejects(runner.finish("claude", started.id, { callback: "code=c1&state=old" }), /older attempt/);
  assert.ok(terms[0].alive(), "a wrong state is refused here: Claude would end its login on it");
  await assert.rejects(runner.finish("claude", "someone-else", { callback: `code=c1&state=${state}` }), /no longer running/);

  const done = await runner.finish("claude", started.id, { callback: `/callback?code=c1&state=${state}` });
  assert.equal(done.status, "done");
  assert.ok(fs.existsSync(path.join(home, "claude-login.json")), "the CLI finished its own login");
  assert.equal(runner.snapshot().claude.status, "done");
  assert.deepEqual(Object.keys(runner.snapshot().claude).sort(), ["endedAt", "error", "id", "mode", "provider", "startedAt", "status"],
    "no URLs or codes in what is broadcast");
  assert.deepEqual(changes, ["claude:waiting", "claude:finishing", "claude:done"]);
});

test("Claude: the code from the sign-in page, pasted on the phone, finishes the same run", async (t) => {
  const { runner, home } = setup(t);
  const started = await runner.start("claude");
  const state = new URL(started.codeUrl).searchParams.get("state");
  const done = await runner.finish("claude", started.id, { code: ` good#${state}\n` });
  assert.equal(done.status, "done");
  assert.ok(fs.existsSync(path.join(home, "claude-login.json")));
});

test("a sign-in the account refused, or a code that is wrong, ends failed with the CLI's reason", async (t) => {
  const { runner } = setup(t);
  let started = await runner.start("claude");
  let state = new URL(started.url).searchParams.get("state");
  const denied = await runner.finish("claude", started.id, { callback: `error=access_denied&error_description=User+declined&state=${state}` });
  assert.equal(denied.status, "failed");
  assert.match(denied.error, /User declined/);

  started = await runner.start("claude");
  state = new URL(started.url).searchParams.get("state");
  const rejected = await runner.finish("claude", started.id, { callback: `code=reject&state=${state}` });
  assert.equal(rejected.status, "failed");
  assert.match(rejected.error, /invalid_grant/);

  started = await runner.start("claude");
  const wrong = await runner.finish("claude", started.id, { code: "nope#nope" });
  assert.equal(wrong.status, "failed");
  assert.match(wrong.error, /Invalid code/);
});

test("Codex: browser callback, device code, and a new start replacing the old one", async (t) => {
  const { runner, home, terms } = setup(t);
  const first = await runner.start("codex");
  assert.equal(first.callback.path, "/auth/callback");
  assert.equal(first.codeUrl, null);
  const second = await runner.start("codex");
  assert.notEqual(second.id, first.id);
  await waitFor(() => !terms[0].alive());
  await assert.rejects(runner.finish("codex", first.id, { callback: "code=x&state=y" }), /no longer running/);
  await assert.rejects(runner.finish("codex", second.id, { code: "abc" }), /device code/);
  const state = new URL(second.url).searchParams.get("state");
  assert.equal((await runner.finish("codex", second.id, { callback: `code=c2&state=${state}` })).status, "done");
  fs.rmSync(path.join(home, "codex-login.json"));

  const device = await runner.start("codex", { device: true });
  assert.equal(device.mode, "device");
  assert.equal(device.userCode, "ABCD-EFGHI");
  assert.equal(device.url, "https://auth.openai.com/codex/device");
  await waitFor(() => runner.snapshot().codex.status === "done");
  assert.ok(fs.existsSync(path.join(home, "codex-login.json")), "the device login finished on its own");
});

test("cancel ends the CLI, and an API key in the machine's env never decides the login", async (t) => {
  const { runner, home, terms } = setup(t);
  const started = await runner.start("claude");
  const env = JSON.parse(fs.readFileSync(path.join(home, "claude-env.json"), "utf8"));
  assert.equal(env.apiKey, null);
  assert.match(env.browser, /browser\.sh$/);
  assert.equal(runner.cancel("claude", started.id).status, "cancelled");
  await waitFor(() => !terms[0].alive());
  assert.equal(runner.snapshot().claude.status, "cancelled");
});

test("a CLI that never shows a link fails the start with what it printed", async (t) => {
  const dir = tmpdir();
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  const bin = path.join(dir, "claude");
  fs.writeFileSync(bin, "#!/bin/sh\necho 'Error: claude is updating, try again'\nexit 3\n", { mode: 0o755 });
  const runner = createLoginRunner({ spawnPty: pipePty, env: process.env, dir: path.join(dir, "l"), cwd: dir, commands: { claude: bin } });
  await assert.rejects(runner.start("claude"), /claude is updating/);
  assert.equal(runner.snapshot().claude.status, "failed");
});
