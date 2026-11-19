import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import "./board-source.mjs";

const { accountEnv, createAccount, listAccounts, removeAccount, setDefaultAccount } = await import("../src/accounts.ts");

test("an added account keeps its own sign-in and shares the rest with the main dir", async (t) => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-accounts-"));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const main = path.join(root, "claude-main");
  const env = { RUDDER_HOME: path.join(root, "rudder"), CLAUDE_CONFIG_DIR: main, CODEX_HOME: path.join(root, "codex-main") };
  fs.mkdirSync(main);
  fs.writeFileSync(path.join(main, "settings.json"), "{}\n");
  fs.writeFileSync(path.join(main, ".credentials.json"), "{\"main\":true}\n");
  fs.writeFileSync(path.join(main, ".claude.json"), JSON.stringify({
    hasCompletedOnboarding: true,
    projects: { "/w": { hasTrustDialogAccepted: true } },
    oauthAccount: { emailAddress: "me@example.com" },
  }));

  const added = await createAccount("claude", env);
  const dir = path.join(env.RUDDER_HOME, "accounts", "claude", added.id);
  assert.equal(added.dir, dir);
  assert.equal(added.default, false);
  assert.equal(fs.readlinkSync(path.join(dir, "projects")), path.join(main, "projects"));
  assert.ok(fs.statSync(path.join(main, "projects")).isDirectory(), "a shared dir the CLI has not made yet is made in the main dir");
  assert.equal(fs.readlinkSync(path.join(dir, "settings.json")), path.join(main, "settings.json"));
  assert.ok(!fs.existsSync(path.join(dir, "CLAUDE.md")), "a shared file the main dir lacks is not linked");
  assert.ok(!fs.existsSync(path.join(dir, ".credentials.json")), "the sign-in is never shared");
  const state = JSON.parse(fs.readFileSync(path.join(dir, ".claude.json"), "utf8"));
  assert.deepEqual(state, { hasCompletedOnboarding: true, projects: { "/w": { hasTrustDialogAccepted: true } } });
  assert.equal(fs.statSync(path.join(dir, ".claude.json")).mode & 0o777, 0o600);

  assert.equal(accountEnv("claude", added.id, env).CLAUDE_CONFIG_DIR, dir);
  assert.equal(accountEnv("claude", "main", env).CLAUDE_CONFIG_DIR, main);
  assert.throws(() => accountEnv("claude", "zzz", env), /no Claude Code account zzz/);
  await assert.rejects(setDefaultAccount("claude", "../x", env), /not an account id/);

  const listed = await listAccounts(env);
  assert.deepEqual(listed.claude.map((a) => [a.id, a.default]), [["main", true], [added.id, false]]);
  assert.deepEqual(listed.codex.map((a) => [a.id, a.dir]), [["main", env.CODEX_HOME]]);

  // No config.json yet: making it the default writes one.
  await setDefaultAccount("claude", added.id, env);
  const configFile = path.join(env.RUDDER_HOME, "config.json");
  assert.equal(JSON.parse(fs.readFileSync(configFile, "utf8")).backends.claude.account, added.id);
  assert.deepEqual((await listAccounts(env)).claude.map((a) => a.default), [false, true]);

  // Removing it drops its dir, never what it linked to, and main is the default again.
  await removeAccount("claude", added.id, env);
  assert.ok(!fs.existsSync(dir));
  assert.ok(fs.existsSync(path.join(main, "settings.json")) && fs.existsSync(path.join(main, "projects")));
  const config = JSON.parse(fs.readFileSync(configFile, "utf8"));
  assert.equal(config.backends.claude.account, undefined);
  assert.equal(config.backends.claude.model, "sonnet", "the rest of the backend's config stays");
  assert.deepEqual((await listAccounts(env)).claude.map((a) => [a.id, a.default]), [["main", true]]);
  await assert.rejects(removeAccount("claude", "main", env), /not an account id/);
});

// Every file that holds a sign-in in the CLI versions the image pins (claude
// 2.1.328, codex 0.170.0). None may be reachable from a new account's dir,
// through a link or a copy: it starts signed out.
const SIGN_IN = {
  claude: [".credentials.json", "mcp-needs-auth-cache.json", "mcp-oauth-tokens.json", "mcp-oauth-tokens-v2.json", "backups"],
  codex: ["auth.json", "auth.lock", "auth.json.lock", ".credentials.json", "gateway_oauth.json", "gateway_oauth.lock", "installation_id", "app-server-daemon", "app-server-control"],
};
const CLAUDE_ACCOUNT_KEYS = ["oauthAccount", "userID", "primaryApiKey", "customApiKeyResponses", "machineID", "orgModelDefaultCache", "passesEligibilityCache"];

/** Every path under `dir`, links followed, as paths relative to it. */
function reachable(dir, at = "") {
  return fs.readdirSync(path.join(dir, at)).flatMap((name) => {
    const rel = path.join(at, name);
    return fs.statSync(path.join(dir, rel)).isDirectory() ? [rel, ...reachable(dir, rel)] : [rel];
  });
}

test("a new account starts signed out, whatever the main one is signed in with", async (t) => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-accounts-"));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const env = { RUDDER_HOME: path.join(root, "rudder"), CLAUDE_CONFIG_DIR: path.join(root, "claude-main"), CODEX_HOME: path.join(root, "codex-main") };
  const mains = { claude: env.CLAUDE_CONFIG_DIR, codex: env.CODEX_HOME };
  for (const [backend, names] of Object.entries(SIGN_IN)) {
    for (const name of names) fs.mkdirSync(path.dirname(path.join(mains[backend], name)), { recursive: true });
    for (const name of names) fs.writeFileSync(path.join(mains[backend], name), "signed in\n");
  }
  // The shared parts exist and are full, as on a machine in use.
  fs.writeFileSync(path.join(env.CLAUDE_CONFIG_DIR, "settings.json"), "{}\n");
  fs.mkdirSync(path.join(env.CLAUDE_CONFIG_DIR, "projects", "-w"), { recursive: true });
  fs.writeFileSync(path.join(env.CLAUDE_CONFIG_DIR, "projects", "-w", "s.jsonl"), "{}\n");
  fs.writeFileSync(path.join(env.CODEX_HOME, "config.toml"), "model = \"x\"\n");
  fs.writeFileSync(path.join(env.CLAUDE_CONFIG_DIR, ".claude.json"),
    JSON.stringify({ hasCompletedOnboarding: true, ...Object.fromEntries(CLAUDE_ACCOUNT_KEYS.map((key) => [key, "secret"])) }));

  for (const backend of ["claude", "codex"]) {
    const account = await createAccount(backend, env);
    const seen = reachable(account.dir);
    assert.ok(seen.length > 0, `${backend}: the shared parts are there`);
    for (const rel of seen) {
      for (const name of SIGN_IN[backend]) {
        assert.notEqual(path.basename(rel), name, `${backend}: ${rel} is a sign-in file reachable from the new account`);
      }
    }
    for (const rel of seen) {
      if (!fs.lstatSync(path.join(account.dir, rel)).isSymbolicLink() && !rel.includes(path.sep)) {
        assert.ok(backend === "claude" && rel === ".claude.json", `${backend}: ${rel} is a copy, not a link`);
      }
    }
    // Each sign-in file name is the account's own: absent until it signs in.
    for (const name of SIGN_IN[backend]) assert.ok(!fs.existsSync(path.join(account.dir, name)), `${backend}: ${name}`);
  }
  const [, claude] = (await listAccounts(env)).claude;
  const state = JSON.parse(fs.readFileSync(path.join(claude.dir, ".claude.json"), "utf8"));
  assert.deepEqual(Object.keys(state), ["hasCompletedOnboarding"], "no account key is copied");
});
