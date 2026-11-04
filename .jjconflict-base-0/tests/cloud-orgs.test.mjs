// Orgs: the store's rules (cloud/src/orgs.ts) over an in-memory SQLite, and
// the HTTP routing of handleOrgApi behind a tiny node http server.
import { test as nodeTest } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import http from "node:http";
import path from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const cloudDir = path.join(path.resolve(here, ".."), "cloud");
const built =
  fs.existsSync(path.join(cloudDir, "dist", "orgs.js")) && fs.existsSync(path.join(cloudDir, "node_modules", "better-sqlite3"));
const test = built
  ? nodeTest
  : (name, fn) => nodeTest(name, { skip: "cloud not built (npm --prefix cloud install && npm --prefix cloud run build)" }, fn);
const lib = built ? await import(path.join(cloudDir, "dist", "orgs.js")) : null;
const Database = built ? createRequire(path.join(cloudDir, "package.json"))("better-sqlite3") : null;

const DAY = 24 * 3600_000;
const alice = { accountId: "alice", email: "alice@example.com" };
const bob = { accountId: "bob", email: "bob@example.com" };
const carol = { accountId: "carol", email: "Carol@Example.com" };

function setup({ withUserTable = false } = {}) {
  const db = new Database(":memory:");
  if (withUserTable) {
    db.exec('create table "user" (id text primary key, email text, name text)');
    db.prepare('insert into "user" (id, email, name) values (?, ?, ?)').run("bob", "bob@auth.example", "Bob Builder");
  }
  let clock = Date.parse("2026-10-20T12:00:00Z");
  const store = lib.createOrgStore(db, { now: () => clock });
  return { db, store, advance: (ms) => (clock += ms) };
}

function expectOrgError(fn, status, pattern) {
  assert.throws(fn, (error) => {
    assert.ok(error instanceof lib.OrgError, `expected OrgError, got ${error}`);
    assert.equal(error.status, status);
    if (pattern) assert.match(error.message, pattern);
    return true;
  });
}

function join(store, orgId, actor, role = "member", email) {
  const invite = store.createInvite(alice, orgId, { role, email });
  return store.acceptInvite(actor, invite.token);
}

test("creating an org makes the creator its only admin", () => {
  const { store } = setup();
  const org = store.createOrg(alice, "  Exla   Labs ");
  assert.match(org.id, /^org_[0-9a-f]{16}$/);
  assert.equal(org.name, "Exla Labs");
  assert.equal(org.role, "admin");
  assert.equal(org.memberCount, 1);
  assert.deepEqual(store.listOrgsForAccount("alice").map((o) => o.id), [org.id]);
  assert.deepEqual(store.listOrgsForAccount("bob"), []);
  assert.equal(store.roleOf("alice", org.id), "admin");
  assert.deepEqual(store.orgIdsForAccount("alice"), [org.id]);
});

test("org names are 1 to 60 characters after trimming", () => {
  const { store } = setup();
  expectOrgError(() => store.createOrg(alice, "   "), 400);
  expectOrgError(() => store.createOrg(alice, 42), 400);
  expectOrgError(() => store.createOrg(alice, "x".repeat(61)), 400);
  assert.equal(store.createOrg(alice, "x".repeat(60)).name.length, 60);
});

test("non-members get 404 and members cannot do admin things", () => {
  const { store } = setup();
  const org = store.createOrg(alice, "Exla");
  expectOrgError(() => store.getOrgView(bob, org.id), 404);
  expectOrgError(() => store.getOrgView(alice, "org_nope"), 404);
  expectOrgError(() => store.renameOrg(bob, org.id, "Mine"), 404);
  join(store, org.id, bob);
  const view = store.getOrgView(bob, org.id);
  assert.equal(view.role, "member");
  assert.deepEqual(view.invites, []);
  expectOrgError(() => store.renameOrg(bob, org.id, "Mine"), 403);
  expectOrgError(() => store.deleteOrg(bob, org.id), 403);
  expectOrgError(() => store.createInvite(bob, org.id, { role: "member" }), 403);
  expectOrgError(() => store.setRole(bob, org.id, "alice", "member"), 403);
  expectOrgError(() => store.removeMember(bob, org.id, "alice"), 403);
});

test("admins rename and delete; delete removes members and invites", () => {
  const { store, db } = setup();
  const org = store.createOrg(alice, "Exla");
  assert.equal(store.renameOrg(alice, org.id, "Exla AI").name, "Exla AI");
  store.createInvite(alice, org.id, { role: "member" });
  store.deleteOrg(alice, org.id);
  assert.deepEqual(store.listOrgsForAccount("alice"), []);
  assert.equal(db.prepare("select count(*) as n from rudder_org_invites").get().n, 0);
  assert.equal(db.prepare("select count(*) as n from rudder_org_members").get().n, 0);
});

test("invite tokens are stored hashed and only returned at creation", () => {
  const { store, db } = setup();
  const org = store.createOrg(alice, "Exla");
  const invite = store.createInvite(alice, org.id, { role: "admin", email: "Bob@Example.com" });
  assert.match(invite.token, /^[A-Za-z0-9_-]{43}$/);
  assert.equal(invite.email, "bob@example.com");
  assert.equal(Date.parse(invite.expiresAt) - Date.parse(invite.createdAt), 7 * DAY);
  const row = db.prepare("select token_hash from rudder_org_invites where id = ?").get(invite.id);
  assert.equal(row.token_hash, lib.hashInviteToken(invite.token));
  assert.notEqual(row.token_hash, invite.token);
  const view = store.getOrgView(alice, org.id);
  assert.deepEqual(view.invites, [
    { id: invite.id, email: "bob@example.com", role: "admin", createdAt: invite.createdAt, expiresAt: invite.expiresAt },
  ]);
  assert.ok(!JSON.stringify(view).includes(invite.token));
  expectOrgError(() => store.createInvite(alice, org.id, { role: "owner" }), 400);
  expectOrgError(() => store.createInvite(alice, org.id, { role: "member", email: "nope" }), 400);
});

test("an invite is single use", () => {
  const { store } = setup();
  const org = store.createOrg(alice, "Exla");
  const invite = store.createInvite(alice, org.id, { role: "member" });
  const info = store.inviteInfo(bob, invite.token);
  assert.deepEqual(info.org, { id: org.id, name: "Exla" });
  assert.equal(info.alreadyMember, false);
  assert.deepEqual(store.acceptInvite(bob, invite.token), { id: org.id, name: "Exla", role: "member" });
  expectOrgError(() => store.acceptInvite(carol, invite.token), 410, /already been used/);
  expectOrgError(() => store.inviteInfo(carol, invite.token), 410);
  assert.deepEqual(store.getOrgView(alice, org.id).invites, []);
});

test("accepting again as an existing member is a no-op", () => {
  const { store } = setup();
  const org = store.createOrg(alice, "Exla");
  const invite = store.createInvite(alice, org.id, { role: "admin" });
  assert.equal(store.inviteInfo(alice, invite.token).alreadyMember, true);
  assert.deepEqual(store.acceptInvite(alice, invite.token), { id: org.id, name: "Exla", role: "admin" });
  // Still usable by the person it was meant for.
  assert.equal(store.acceptInvite(bob, invite.token).role, "admin");
  assert.equal(store.acceptInvite(bob, invite.token).role, "admin");
});

test("invites expire after 7 days", () => {
  const { store, advance } = setup();
  const org = store.createOrg(alice, "Exla");
  const invite = store.createInvite(alice, org.id, { role: "member" });
  advance(7 * DAY);
  expectOrgError(() => store.inviteInfo(bob, invite.token), 410, /expired/);
  expectOrgError(() => store.acceptInvite(bob, invite.token), 410, /expired/);
  assert.deepEqual(store.getOrgView(alice, org.id).invites, []);
});

test("unknown and revoked invites are 404", () => {
  const { store } = setup();
  const org = store.createOrg(alice, "Exla");
  expectOrgError(() => store.inviteInfo(bob, "nope"), 404);
  const invite = store.createInvite(alice, org.id, { role: "member" });
  expectOrgError(() => store.revokeInvite(alice, org.id, "inv_nope"), 404);
  store.revokeInvite(alice, org.id, invite.id);
  expectOrgError(() => store.acceptInvite(bob, invite.token), 404);
  const other = store.createInvite(alice, org.id, { role: "member" });
  store.deleteOrg(alice, org.id);
  expectOrgError(() => store.inviteInfo(bob, other.token), 404);
});

test("an email-locked invite only works for that email", () => {
  const { store } = setup();
  const org = store.createOrg(alice, "Exla");
  const invite = store.createInvite(alice, org.id, { role: "member", email: "carol@example.com" });
  expectOrgError(() => store.acceptInvite(bob, invite.token), 403, /carol@example\.com/);
  expectOrgError(() => store.acceptInvite({ accountId: "nomail" }, invite.token), 403);
  // Case-insensitive match; the failed tries did not burn it.
  assert.equal(store.acceptInvite(carol, invite.token).role, "member");
});

test("an email-locked invite falls back to the better-auth email", () => {
  const { store } = setup({ withUserTable: true });
  const org = store.createOrg(alice, "Exla");
  const invite = store.createInvite(alice, org.id, { role: "member", email: "bob@auth.example" });
  assert.equal(store.acceptInvite({ accountId: "bob" }, invite.token).role, "member");
});

test("member email and name come from the better-auth user table, else the join email", () => {
  const { store } = setup({ withUserTable: true });
  const org = store.createOrg(alice, "Exla");
  join(store, org.id, bob);
  const members = store.getOrgView(alice, org.id).members;
  assert.deepEqual(
    members.map((m) => [m.accountId, m.email, m.name, m.role, m.you]),
    [
      ["alice", "alice@example.com", null, "admin", true],
      ["bob", "bob@auth.example", "Bob Builder", "member", false],
    ],
  );
  assert.ok(Date.parse(members[0].joinedAt));
});

test("an org always keeps one admin", () => {
  const { store } = setup();
  const org = store.createOrg(alice, "Exla");
  expectOrgError(() => store.setRole(alice, org.id, "alice", "member"), 409, /at least one admin/);
  expectOrgError(() => store.removeMember(alice, org.id, "alice"), 409, /at least one admin/);
  join(store, org.id, bob);
  store.setRole(alice, org.id, "bob", "admin");
  assert.equal(store.roleOf("bob", org.id), "admin");
  // Two admins: alice can step down or leave.
  assert.equal(store.setRole(alice, org.id, "alice", "member"), "member");
  expectOrgError(() => store.setRole(bob, org.id, "bob", "member"), 409);
  store.removeMember(bob, org.id, "alice");
  assert.equal(store.roleOf("alice", org.id), null);
  expectOrgError(() => store.removeMember(bob, org.id, "bob"), 409);
  expectOrgError(() => store.setRole(bob, org.id, "ghost", "admin"), 404);
  expectOrgError(() => store.setRole(bob, org.id, "bob", "owner"), 400);
});

test("a member can leave; an admin can remove anyone", () => {
  const { store } = setup();
  const org = store.createOrg(alice, "Exla");
  join(store, org.id, bob);
  join(store, org.id, carol);
  expectOrgError(() => store.removeMember(bob, org.id, "carol"), 403);
  store.removeMember(bob, org.id, "bob");
  assert.equal(store.roleOf("bob", org.id), null);
  expectOrgError(() => store.getOrgView(bob, org.id), 404);
  store.removeMember(alice, org.id, "carol");
  assert.equal(store.listOrgsForAccount("alice")[0].memberCount, 1);
  expectOrgError(() => store.removeMember(alice, org.id, "carol"), 404);
});

// HTTP: handleOrgApi behind a real server with fake deps.

async function withServer(fn, { withUserTable = false } = {}) {
  const { store } = setup({ withUserTable });
  const captured = [];
  const deps = {
    store,
    baseURL: "https://cloud.example/",
    sendJson: (res, status, body) => {
      res.writeHead(status, { "content-type": "application/json" });
      res.end(JSON.stringify(body));
    },
    readJsonBody: async (req) => {
      const chunks = [];
      for await (const chunk of req) chunks.push(chunk);
      return chunks.length ? JSON.parse(Buffer.concat(chunks).toString("utf8")) : {};
    },
    capture: (accountId, name, props) => captured.push({ accountId, name, props }),
  };
  const server = http.createServer(async (req, res) => {
    const url = new URL(req.url, "http://x");
    const accountId = req.headers["x-account"] || "anon";
    const account = { accountId, email: `${accountId}@example.com` };
    try {
      const handled = await lib.handleOrgApi(req, res, url, account, deps);
      if (!handled) deps.sendJson(res, 418, { error: "not mine" });
    } catch (error) {
      deps.sendJson(res, 500, { error: String(error) });
    }
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const base = `http://127.0.0.1:${server.address().port}`;
  const call = async (method, pathname, as, body) => {
    const response = await fetch(base + pathname, {
      method,
      headers: { "x-account": as, ...(body ? { "content-type": "application/json" } : {}) },
      body: body === undefined ? undefined : typeof body === "string" ? body : JSON.stringify(body),
    });
    const text = await response.text();
    return { status: response.status, body: text ? JSON.parse(text) : null };
  };
  try {
    await fn(call, captured);
  } finally {
    await new Promise((resolve) => server.close(resolve));
  }
}

test("HTTP: the whole org flow through handleOrgApi", async () => {
  await withServer(async (call, captured) => {
    assert.equal((await call("GET", "/api/app/other", "alice")).status, 418);
    assert.deepEqual((await call("GET", "/api/app/orgs", "alice")).body, { orgs: [] });

    const created = await call("POST", "/api/app/orgs", "alice", { name: "Exla" });
    assert.equal(created.status, 200);
    const org = created.body.org;
    assert.equal(org.role, "admin");
    assert.equal(org.memberCount, 1);
    assert.equal((await call("POST", "/api/app/orgs", "alice", { name: "" })).status, 400);
    assert.equal((await call("POST", "/api/app/orgs", "alice", "{nope")).status, 400);

    const inv = await call("POST", `/api/app/orgs/${org.id}/invites`, "alice", { role: "member" });
    assert.equal(inv.status, 200);
    const token = inv.body.invite.url.split("/app/join/")[1];
    assert.equal(inv.body.invite.url, `https://cloud.example/app/join/${token}`);
    assert.equal(inv.body.invite.token, undefined);
    assert.equal((await call("POST", `/api/app/orgs/${org.id}/invites`, "bob", { role: "member" })).status, 404);

    const info = await call("GET", `/api/app/invites/${token}`, "bob");
    assert.equal(info.status, 200);
    assert.deepEqual(info.body.org, { id: org.id, name: "Exla" });
    assert.equal(info.body.alreadyMember, false);
    assert.equal((await call("GET", "/api/app/invites/unknown", "bob")).status, 404);

    const accepted = await call("POST", `/api/app/invites/${token}/accept`, "bob");
    assert.deepEqual(accepted.body, { org: { id: org.id, name: "Exla", role: "member" } });
    assert.equal((await call("POST", `/api/app/invites/${token}/accept`, "carol")).status, 410);

    const view = await call("GET", `/api/app/orgs/${org.id}`, "bob");
    assert.equal(view.body.role, "member");
    assert.deepEqual(view.body.members.map((m) => [m.accountId, m.you]), [["alice", false], ["bob", true]]);
    assert.equal((await call("GET", `/api/app/orgs/${org.id}`, "carol")).status, 404);
    assert.equal((await call("PATCH", `/api/app/orgs/${org.id}`, "bob", { name: "x" })).status, 403);
    assert.equal((await call("PATCH", `/api/app/orgs/${org.id}`, "alice", { name: "Exla AI" })).body.org.name, "Exla AI");

    assert.equal((await call("DELETE", `/api/app/orgs/${org.id}/members/alice`, "alice")).status, 409);
    const promoted = await call("POST", `/api/app/orgs/${org.id}/members/bob`, "alice", { role: "admin" });
    assert.equal(promoted.status, 200);
    assert.equal((await call("DELETE", `/api/app/orgs/${org.id}/members/alice`, "alice")).status, 200);
    assert.equal((await call("GET", `/api/app/orgs/${org.id}`, "alice")).status, 404);

    const inv2 = await call("POST", `/api/app/orgs/${org.id}/invites`, "bob", { role: "member", email: "dan@example.com" });
    const token2 = inv2.body.invite.url.split("/app/join/")[1];
    assert.equal((await call("POST", `/api/app/invites/${token2}/accept`, "erin")).status, 403);
    assert.equal((await call("DELETE", `/api/app/orgs/${org.id}/invites/${inv2.body.invite.id}`, "bob")).status, 200);
    assert.equal((await call("POST", `/api/app/invites/${token2}/accept`, "dan")).status, 404);

    assert.equal((await call("PUT", `/api/app/orgs/${org.id}`, "bob")).status, 405);
    assert.equal((await call("GET", `/api/app/orgs/${org.id}/nothing`, "bob")).status, 404);
    assert.deepEqual((await call("DELETE", `/api/app/orgs/${org.id}`, "bob")).body, { ok: true });
    assert.deepEqual((await call("GET", "/api/app/orgs", "bob")).body, { orgs: [] });

    assert.deepEqual(
      captured.map((c) => c.name),
      ["org_created", "org_invite_created", "org_invite_accepted", "org_member_role_changed", "org_member_removed", "org_invite_created"],
    );
    assert.ok(captured.every((c) => !JSON.stringify(c.props ?? {}).includes("@")));
  });
});
