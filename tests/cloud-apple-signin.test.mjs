// Sign in with Apple from the iPhone app (POST /api/app/apple-signin) against
// the real control plane. A key pair made here stands in for Apple's: the
// server reads its published keys from APPLE_JWKS_URL, which production points
// at Apple.
import { test as nodeTest } from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createHash, createSign, generateKeyPairSync } from "node:crypto";
import { createRequire } from "node:module";
import http from "node:http";
import net from "node:net";
import os from "node:os";
import path from "node:path";
import fs from "node:fs";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const cloudDir = path.join(path.resolve(here, ".."), "cloud");
const cloudReady =
  fs.existsSync(path.join(cloudDir, "dist", "server.js")) &&
  fs.existsSync(path.join(cloudDir, "node_modules", "better-sqlite3"));
const test = cloudReady
  ? nodeTest
  : (name, options, fn) =>
      nodeTest(name, { skip: "cloud not built (npm --prefix cloud install && npm --prefix cloud run build)" }, fn);
const Database = cloudReady ? createRequire(path.join(cloudDir, "package.json"))("better-sqlite3") : null;

const BUNDLE_ID = "ai.exla.rudder.test";
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const sha256 = (text) => createHash("sha256").update(text).digest("hex");
const tokenHash = (token) => sha256(token).slice(0, 32);
const b64 = (value) => Buffer.from(JSON.stringify(value)).toString("base64url");

async function freePort() {
  return new Promise((resolve, reject) => {
    const srv = net.createServer();
    srv.unref();
    srv.on("error", reject);
    srv.listen(0, "127.0.0.1", () => {
      const { port } = srv.address();
      srv.close(() => resolve(port));
    });
  });
}

async function waitFor(fn, { timeout = 8000, interval = 100 } = {}) {
  const start = Date.now();
  for (;;) {
    try {
      const v = await fn();
      if (v) return v;
    } catch {
      // keep polling
    }
    if (Date.now() - start > timeout) throw new Error("waitFor timed out");
    await sleep(interval);
  }
}

test("an Apple identity token signs the iPhone app in, and only a real one", { timeout: 30_000 }, async (t) => {
  const apple = generateKeyPairSync("rsa", { modulusLength: 2048 });
  const stranger = generateKeyPairSync("rsa", { modulusLength: 2048 });
  const jwks = http.createServer((req, res) => {
    res.writeHead(200, { "content-type": "application/json" });
    res.end(JSON.stringify({ keys: [{ ...apple.publicKey.export({ format: "jwk" }), kid: "test-key", alg: "RS256", use: "sig" }] }));
  });
  await new Promise((resolve, reject) => {
    jwks.once("error", reject);
    jwks.listen(0, "127.0.0.1", resolve);
  });

  const port = await freePort();
  const dataDir = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-apple-signin-test-"));
  const dbPath = path.join(dataDir, "state.sqlite");
  const base = `http://127.0.0.1:${port}`;
  const child = spawn(process.execPath, [path.join(cloudDir, "dist", "server.js")], {
    cwd: cloudDir,
    env: {
      ...process.env,
      PORT: String(port),
      BETTER_AUTH_URL: base,
      BETTER_AUTH_SECRET: "test-secret-test-secret-test-secret",
      RUDDER_CLOUD_DATA_DIR: dataDir,
      RUDDER_CLOUD_DB: dbPath,
      RUDDER_CLOUD_PERSIST_STATE: "0",
      RUDDER_S3_BUCKET: "",
      RUDDER_FABRIC_S3_BUCKET: "",
      SLACK_BOT_TOKEN: "",
      APPLE_APP_BUNDLE_ID: BUNDLE_ID,
      APPLE_JWKS_URL: `http://127.0.0.1:${jwks.address().port}/auth/keys`,
    },
    stdio: ["ignore", "pipe", "pipe"],
  });
  let log = "";
  child.stdout.on("data", (d) => { log += d.toString(); });
  child.stderr.on("data", (d) => { log += d.toString(); });
  t.after(async () => {
    child.kill("SIGKILL");
    jwks.closeAllConnections();
    await new Promise((resolve) => jwks.close(resolve));
    fs.rmSync(dataDir, { recursive: true, force: true });
  });
  await waitFor(async () => (await fetch(`${base}/health`).catch(() => null))?.ok);
  await waitFor(() => fs.existsSync(dbPath));
  await sleep(150);
  const db = new Database(dbPath);
  t.after(() => db.close());

  // What iOS hands the app: Apple's claims, with the SHA-256 of the app's nonce.
  const identityToken = (nonce, claims = {}, key = apple.privateKey) => {
    const seconds = Math.floor(Date.now() / 1000);
    const signed = `${b64({ alg: "RS256", kid: "test-key" })}.${b64({
      iss: "https://appleid.apple.com",
      aud: BUNDLE_ID,
      sub: "001234.apple-user",
      iat: seconds,
      exp: seconds + 600,
      nonce: sha256(nonce),
      email: "sam@example.com",
      email_verified: "true",
      ...claims,
    })}`;
    return `${signed}.${createSign("RSA-SHA256").update(signed).sign(key).toString("base64url")}`;
  };
  const signIn = (body) => fetch(`${base}/api/app/apple-signin`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body),
  });
  const accountOf = (token) => db.prepare("select account_id, email from rudder_tokens where token_hash = ?").get(tokenHash(token));

  assert.equal((await (await fetch(`${base}/health`)).json()).auth.apple, true);
  assert.equal((await fetch(`${base}/api/app/apple-signin`)).status, 405);
  assert.equal((await signIn({ identityToken: identityToken("n") })).status, 400, "the nonce is required");

  // First sign-in: a new account, with the name Apple gives only this once.
  const first = await signIn({ identityToken: identityToken("nonce-1"), nonce: "nonce-1", givenName: "Sam", familyName: "Rivera" });
  assert.equal(first.status, 200, log);
  const firstBody = await first.json();
  assert.match(firstBody.token, /^rdr_/);
  assert.equal(firstBody.email, "sam@example.com");
  const account = accountOf(firstBody.token);
  assert.equal(account.email, "sam@example.com");
  assert.deepEqual(
    db.prepare(`select name, email, emailVerified from "user" where id = ?`).get(account.account_id),
    { name: "Sam Rivera", email: "sam@example.com", emailVerified: 1 },
  );
  assert.deepEqual(
    db.prepare("select providerId, accountId from account where userId = ?").all(account.account_id),
    [{ providerId: "apple", accountId: "001234.apple-user" }],
  );
  const state = await fetch(`${base}/api/app/state`, { headers: { authorization: `Bearer ${firstBody.token}` } });
  assert.equal(state.status, 200, log);
  assert.equal((await state.json()).email, "sam@example.com");

  // Again, with no name this time: the same account.
  const again = await signIn({ identityToken: identityToken("nonce-2"), nonce: "nonce-2" });
  assert.equal(again.status, 200, log);
  assert.equal(accountOf((await again.json()).token).account_id, account.account_id);
  assert.equal(db.prepare(`select count(*) as n from "user"`).get().n, 1);

  // Someone who already signed in with Google under the same verified address
  // lands in that account, not a second one.
  const now = new Date().toISOString();
  db.prepare(`insert into "user" (id, name, email, emailVerified, createdAt, updatedAt) values ('user-google', 'Kit', 'kit@example.com', 1, ?, ?)`).run(now, now);
  db.prepare("insert into account (id, accountId, providerId, userId, createdAt, updatedAt) values ('link-google', 'g-1', 'google', 'user-google', ?, ?)").run(now, now);
  const joined = await signIn({ identityToken: identityToken("nonce-3", { sub: "005678.kit", email: "kit@example.com" }), nonce: "nonce-3" });
  assert.equal(joined.status, 200, log);
  assert.equal(accountOf((await joined.json()).token).account_id, "user-google");
  assert.deepEqual(
    db.prepare("select providerId from account where userId = 'user-google' order by providerId").all().map((row) => row.providerId),
    ["apple", "google"],
  );

  // Everything else is refused with one answer and no token.
  const tokensBefore = db.prepare("select count(*) as n from rudder_tokens").get().n;
  const seconds = Math.floor(Date.now() / 1000);
  const refusals = {
    "a nonce that is not this sign-in's": { identityToken: identityToken("nonce-4"), nonce: "another-nonce" },
    "a token made for another app": { identityToken: identityToken("nonce-4", { aud: "com.example.other" }), nonce: "nonce-4" },
    "a token from another issuer": { identityToken: identityToken("nonce-4", { iss: "https://example.com" }), nonce: "nonce-4" },
    "a token that has expired": { identityToken: identityToken("nonce-4", { iat: seconds - 1200, exp: seconds - 600 }), nonce: "nonce-4" },
    "a token Apple did not sign": { identityToken: identityToken("nonce-4", {}, stranger.privateKey), nonce: "nonce-4" },
    "an address Apple has not verified": { identityToken: identityToken("nonce-4", { email: "kit@example.com", sub: "009.unverified", email_verified: "false" }), nonce: "nonce-4" },
    "an unsigned token": { identityToken: `${b64({ alg: "none", kid: "test-key" })}.${b64({ iss: "https://appleid.apple.com", aud: BUNDLE_ID, sub: "001234.apple-user", exp: seconds + 600, nonce: sha256("nonce-4"), email: "sam@example.com", email_verified: "true" })}.`, nonce: "nonce-4" },
  };
  for (const [what, body] of Object.entries(refusals)) {
    const res = await signIn(body);
    assert.equal(res.status, 401, `${what} was not refused`);
    assert.deepEqual(await res.json(), { error: "Apple could not confirm this sign-in. Try again." });
  }
  assert.equal(db.prepare("select count(*) as n from rudder_tokens").get().n, tokensBefore);
  assert.equal(db.prepare(`select count(*) as n from "user"`).get().n, 2);
});
