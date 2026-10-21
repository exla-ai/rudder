#!/usr/bin/env node
// Google Cloud helper for agents. No dependencies; Node 18+.
//
// Does everything Google exposes an API for (identity, projects, APIs) and,
// for the parts that only exist in the Cloud Console (OAuth branding and
// clients), prints the exact pages and values, then verifies the result from
// the outside the way a user would see it.
//
//   node gcp.mjs whoami
//   node gcp.mjs projects [--filter text]
//   node gcp.mjs ensure-project --id tempo-exla --name "Tempo" [--org 123 | --folder 456]
//   node gcp.mjs enable --project tempo-exla --apis gmail,calendar
//   node gcp.mjs audit --project tempo-exla
//   node gcp.mjs oauth-plan --project tempo-exla --name "Tempo" --home https://tempo.example.com \
//        --support you@example.com --redirect https://tempo.example.com/api/auth/callback/google [--redirect …] \
//        [--scopes gmail.readonly,calendar.readonly] [--privacy URL] [--terms URL] [--domain example.com]
//   node gcp.mjs check-client --client-id ID --redirect URI [--redirect …] [--expect-name "Tempo"] [--scopes …]
//
// Auth, in order: GOOGLE_OAUTH_ACCESS_TOKEN, `gcloud auth print-access-token`,
// gcloud's stored user credentials, application-default credentials.

import { execFileSync } from "node:child_process";
import { existsSync, readdirSync, readFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

const API_ALIASES = {
  gmail: "gmail.googleapis.com",
  calendar: "calendar-json.googleapis.com",
  drive: "drive.googleapis.com",
  sheets: "sheets.googleapis.com",
  docs: "docs.googleapis.com",
  people: "people.googleapis.com",
  contacts: "people.googleapis.com",
  youtube: "youtube.googleapis.com",
  maps: "maps-backend.googleapis.com",
  places: "places.googleapis.com",
  gemini: "generativelanguage.googleapis.com",
  vertex: "aiplatform.googleapis.com",
  firebase: "firebase.googleapis.com",
  run: "run.googleapis.com",
  storage: "storage.googleapis.com",
  secretmanager: "secretmanager.googleapis.com",
  pubsub: "pubsub.googleapis.com",
};

// Scope shorthands → full scope, and Google's sensitivity class (drives verification).
const SCOPES = {
  "openid": ["openid", "basic"],
  "email": ["https://www.googleapis.com/auth/userinfo.email", "basic"],
  "profile": ["https://www.googleapis.com/auth/userinfo.profile", "basic"],
  "gmail.readonly": ["https://www.googleapis.com/auth/gmail.readonly", "restricted"],
  "gmail.modify": ["https://www.googleapis.com/auth/gmail.modify", "restricted"],
  "gmail.compose": ["https://www.googleapis.com/auth/gmail.compose", "restricted"],
  "gmail.send": ["https://www.googleapis.com/auth/gmail.send", "sensitive"],
  "gmail.metadata": ["https://www.googleapis.com/auth/gmail.metadata", "restricted"],
  "calendar.readonly": ["https://www.googleapis.com/auth/calendar.readonly", "sensitive"],
  "calendar.events": ["https://www.googleapis.com/auth/calendar.events", "sensitive"],
  "calendar": ["https://www.googleapis.com/auth/calendar", "sensitive"],
  "drive.file": ["https://www.googleapis.com/auth/drive.file", "basic"],
  "drive.readonly": ["https://www.googleapis.com/auth/drive.readonly", "restricted"],
  "drive": ["https://www.googleapis.com/auth/drive", "restricted"],
  "contacts.readonly": ["https://www.googleapis.com/auth/contacts.readonly", "sensitive"],
};

function parseArgs(argv) {
  const [command, ...rest] = argv;
  const flags = {};
  for (let i = 0; i < rest.length; i++) {
    const a = rest[i];
    if (!a.startsWith("--")) continue;
    const key = a.slice(2);
    const value = rest[i + 1] && !rest[i + 1].startsWith("--") ? rest[++i] : "true";
    (flags[key] ??= []).push(value);
  }
  const one = (k) => flags[k]?.[flags[k].length - 1];
  const many = (k) => (flags[k] ?? []).flatMap((v) => v.split(",")).map((v) => v.trim()).filter(Boolean);
  return { command, one, many };
}

function fail(message) {
  console.error(`✗ ${message}`);
  process.exit(1);
}

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

function gcloudConfigDir() {
  return process.env.CLOUDSDK_CONFIG || join(homedir(), ".config", "gcloud");
}

function activeAccount() {
  try {
    return execFileSync("gcloud", ["config", "get-value", "account"], { encoding: "utf8", stdio: ["ignore", "pipe", "ignore"] }).trim() || null;
  } catch {}
  const dir = gcloudConfigDir();
  try {
    const active = readFileSync(join(dir, "active_config"), "utf8").trim() || "default";
    const config = readFileSync(join(dir, "configurations", `config_${active}`), "utf8");
    return config.match(/^\s*account\s*=\s*(.+)$/m)?.[1]?.trim() ?? null;
  } catch {
    return null;
  }
}

async function refresh(cred) {
  const response = await fetch("https://oauth2.googleapis.com/token", {
    method: "POST",
    headers: { "content-type": "application/x-www-form-urlencoded" },
    body: new URLSearchParams({ client_id: cred.client_id, client_secret: cred.client_secret, refresh_token: cred.refresh_token, grant_type: "refresh_token" }),
  });
  const data = await response.json();
  if (!response.ok) throw new Error(`token refresh failed: ${data.error_description || data.error}`);
  return data.access_token;
}

let cachedToken;
async function token() {
  if (cachedToken) return cachedToken;
  if (process.env.GOOGLE_OAUTH_ACCESS_TOKEN) return (cachedToken = process.env.GOOGLE_OAUTH_ACCESS_TOKEN);
  try {
    const t = execFileSync("gcloud", ["auth", "print-access-token"], { encoding: "utf8", stdio: ["ignore", "pipe", "ignore"] }).trim();
    if (t) return (cachedToken = t);
  } catch {}
  const dir = gcloudConfigDir();
  const account = activeAccount();
  const candidates = [];
  if (account) candidates.push(join(dir, "legacy_credentials", account, "adc.json"));
  if (existsSync(join(dir, "legacy_credentials"))) {
    for (const a of readdirSync(join(dir, "legacy_credentials"))) candidates.push(join(dir, "legacy_credentials", a, "adc.json"));
  }
  candidates.push(process.env.GOOGLE_APPLICATION_CREDENTIALS ?? "", join(dir, "application_default_credentials.json"));
  for (const file of candidates) {
    if (!file || !existsSync(file)) continue;
    const cred = JSON.parse(readFileSync(file, "utf8"));
    if (cred.type === "authorized_user") return (cachedToken = await refresh(cred));
  }
  fail("No Google credentials. Ask the user to run `! gcloud auth login` (and `! gcloud auth application-default login` for ADC), or install the SDK: https://cloud.google.com/sdk/docs/install");
}

async function api(method, url, body) {
  const response = await fetch(url, {
    method,
    headers: { Authorization: `Bearer ${await token()}`, "content-type": "application/json" },
    body: body ? JSON.stringify(body) : undefined,
  });
  const text = await response.text();
  const data = text ? JSON.parse(text) : {};
  if (!response.ok) {
    const err = new Error(`${method} ${url.replace(/^https:\/\//, "")}: ${response.status} ${data.error?.message ?? text.slice(0, 300)}`);
    err.status = response.status;
    throw err;
  }
  return data;
}

async function wait(operation, base) {
  let op = operation;
  for (let i = 0; i < 60 && !op.done; i++) {
    await new Promise((r) => setTimeout(r, 2000));
    op = await api("GET", `${base}/${op.name}`);
  }
  if (op.error) throw new Error(`operation failed: ${op.error.message}`);
  return op;
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

async function whoami() {
  const info = await api("GET", "https://www.googleapis.com/oauth2/v3/userinfo").catch(() => null);
  const tokenInfo = await (await fetch(`https://oauth2.googleapis.com/tokeninfo?access_token=${await token()}`)).json();
  console.log(`account: ${info?.email ?? tokenInfo.email ?? activeAccount() ?? "unknown"}`);
  console.log(`scopes:  ${(tokenInfo.scope ?? "").split(" ").map((s) => s.replace("https://www.googleapis.com/auth/", "")).join(", ")}`);
  const orgs = await api("POST", "https://cloudresourcemanager.googleapis.com/v1/organizations:search", {}).catch(() => ({}));
  for (const o of orgs.organizations ?? []) console.log(`org:     ${o.displayName} (${o.name.split("/")[1]})`);
}

async function listProjects(filter) {
  const out = [];
  let pageToken;
  do {
    const page = await api("GET", `https://cloudresourcemanager.googleapis.com/v1/projects?pageSize=500${pageToken ? `&pageToken=${pageToken}` : ""}`);
    out.push(...(page.projects ?? []));
    pageToken = page.nextPageToken;
  } while (pageToken);
  return out.filter((p) => p.lifecycleState === "ACTIVE" && (!filter || `${p.projectId} ${p.name}`.toLowerCase().includes(filter.toLowerCase())));
}

async function projects(filter) {
  const list = await listProjects(filter);
  for (const p of list) console.log(`${p.projectId.padEnd(34)} ${JSON.stringify(p.name).padEnd(30)} ${p.parent ? `${p.parent.type}/${p.parent.id}` : "no parent"}`);
  console.log(`${list.length} project(s)`);
}

const PROJECT_ID = /^[a-z][a-z0-9-]{4,28}[a-z0-9]$/;

async function ensureProject(id, name, org, folder) {
  if (!id || !name) fail("--id and --name are required (name is what people see; use the product's name)");
  if (!PROJECT_ID.test(id)) fail(`"${id}" is not a valid project ID: 6-30 chars, lowercase letters, digits, hyphens, starts with a letter`);
  if (/^(my first project|untitled|test|default|new project)$/i.test(name)) fail(`"${name}" is a placeholder; name the project after the product`);
  let existing = await api("GET", `https://cloudresourcemanager.googleapis.com/v3/projects/${id}`).catch((e) => (e.status === 403 || e.status === 404 ? null : Promise.reject(e)));
  if (existing && existing.state !== "ACTIVE") fail(`${id} exists but is ${existing.state}; pick another ID or restore it`);
  if (!existing) {
    if (!org && !folder) {
      const orgs = (await api("POST", "https://cloudresourcemanager.googleapis.com/v1/organizations:search", {}).catch(() => ({}))).organizations ?? [];
      if (orgs.length === 1) org = orgs[0].name.split("/")[1];
    }
    const parent = folder ? `folders/${folder}` : org ? `organizations/${org}` : undefined;
    console.log(`creating ${id} "${name}"${parent ? ` under ${parent}` : ""}…`);
    try {
      await wait(await api("POST", "https://cloudresourcemanager.googleapis.com/v3/projects", { projectId: id, displayName: name, ...(parent ? { parent } : {}) }), "https://cloudresourcemanager.googleapis.com/v3");
    } catch (error) {
      if (/already exists|ALREADY_EXISTS/i.test(error.message)) fail(`project ID ${id} is taken globally (by someone else); choose another, e.g. ${id}-app`);
      throw error;
    }
    existing = await api("GET", `https://cloudresourcemanager.googleapis.com/v3/projects/${id}`);
  } else if (existing.displayName !== name) {
    console.log(`renaming ${id}: "${existing.displayName}" → "${name}"`);
    await wait(await api("PATCH", `https://cloudresourcemanager.googleapis.com/v3/projects/${id}?updateMask=displayName`, { displayName: name }), "https://cloudresourcemanager.googleapis.com/v3");
    existing = await api("GET", `https://cloudresourcemanager.googleapis.com/v3/projects/${id}`);
  }
  console.log(`✓ project ${existing.projectId} "${existing.displayName}" (number ${existing.name.split("/")[1]}, parent ${existing.parent ?? "none"})`);
}

function serviceIds(list) {
  return list.map((a) => API_ALIASES[a] ?? (a.includes(".") ? a : `${a}.googleapis.com`));
}

async function enable(project, apis) {
  if (!project || !apis.length) fail("--project and --apis are required");
  const ids = serviceIds(apis);
  await wait(await api("POST", `https://serviceusage.googleapis.com/v1/projects/${project}/services:batchEnable`, { serviceIds: ids }), "https://serviceusage.googleapis.com/v1");
  console.log(`✓ enabled on ${project}: ${ids.join(", ")}`);
}

async function audit(project) {
  if (!project) fail("--project is required");
  const p = await api("GET", `https://cloudresourcemanager.googleapis.com/v3/projects/${project}`);
  console.log(`project:  ${p.projectId}  name "${p.displayName}"  state ${p.state}  parent ${p.parent ?? "none"}`);
  if (/^(my first project|untitled)/i.test(p.displayName) || p.displayName === p.projectId) {
    console.log(`  ! the display name is a placeholder; run ensure-project --id ${p.projectId} --name "<Product>"`);
  }
  const services = [];
  let pageToken;
  do {
    const page = await api("GET", `https://serviceusage.googleapis.com/v1/projects/${project}/services?filter=state:ENABLED&pageSize=200${pageToken ? `&pageToken=${pageToken}` : ""}`);
    services.push(...(page.services ?? []).map((s) => s.config.name));
    pageToken = page.nextPageToken;
  } while (pageToken);
  console.log(`apis:     ${services.filter((s) => !/^(bigquery|cloudapis|cloudtrace|datastore|logging|monitoring|servicemanagement|serviceusage|sql-component|storage-api|storage-component|cloudasset|dataform|dataplex|analyticshub|bigquery\w*)\./.test(s)).join(", ") || "(only defaults)"}`);
  console.log(`console:  https://console.cloud.google.com/auth/overview?project=${project}`);
  console.log("note:     OAuth branding and clients have no public API; check them in the Console or with check-client.");
}

function oauthPlan(o) {
  const project = o.one("project");
  const name = o.one("name");
  const home = o.one("home");
  const support = o.one("support");
  const redirects = o.many("redirect");
  if (!project || !name || !home || !support || !redirects.length) fail("--project, --name, --home, --support and at least one --redirect are required");
  const homeUrl = new URL(home);
  const domain = o.one("domain") ?? homeUrl.hostname.split(".").slice(-2).join(".");
  const privacy = o.one("privacy") ?? `${homeUrl.origin}/privacy`;
  const terms = o.one("terms") ?? `${homeUrl.origin}/terms`;
  const scopes = ["openid", "email", "profile", ...o.many("scopes")];
  const resolved = scopes.map((s) => [s, ...(SCOPES[s] ?? [s.startsWith("http") ? s : `https://www.googleapis.com/auth/${s}`, "unknown"])]);
  const worst = resolved.some((r) => r[2] === "restricted") ? "restricted" : resolved.some((r) => r[2] === "sensitive") ? "sensitive" : "basic";
  const bad = redirects.filter((r) => {
    try {
      const u = new URL(r);
      return !(u.protocol === "https:" || (u.protocol === "http:" && /^(localhost|127\.0\.0\.1|\[::1\])$/.test(u.hostname)));
    } catch {
      return true;
    }
  });
  if (bad.length) fail(`redirect URIs must be https (or http://localhost): ${bad.join(", ")}`);
  const base = "https://console.cloud.google.com/auth";
  const q = `?project=${project}`;
  console.log(`OAuth setup for "${name}" in project ${project}. None of this has a public API: do it in the Console`);
  console.log(`(Claude in Chrome / the user's browser), signed in as an owner of ${project}. Check the account chip first.\n`);
  console.log(`1. Branding        ${base}/branding${q}`);
  console.log(`   App name ............ ${name}        ← exactly what users see: "Sign in to continue to ${name}"`);
  console.log(`   User support email .. ${support}`);
  console.log(`   App logo ............ optional (a logo sends the app to brand verification; skip it until you verify)`);
  console.log(`   Application home .... ${homeUrl.origin}`);
  console.log(`   Privacy policy ...... ${privacy}`);
  console.log(`   Terms of service .... ${terms}`);
  console.log(`   Authorized domains .. ${domain}`);
  console.log(`   Developer contact ... ${support}`);
  console.log(`2. Audience        ${base}/audience${q}`);
  console.log(`   User type External (Internal only lets accounts in your Workspace sign in).`);
  console.log(worst === "basic"
    ? "   Publishing status: Publish app → In production (basic scopes need no review)."
    : `   Keep "Testing" and add test users until verified; in Testing, refresh tokens expire after 7 days and only test users can sign in.`);
  console.log(`3. Data access     ${base}/scopes${q}`);
  for (const [short, full, kind] of resolved) console.log(`   ${kind.padEnd(10)} ${full === "openid" ? "openid" : full}${short !== full ? `  (${short})` : ""}`);
  if (worst === "restricted") console.log("   Restricted scopes need Google verification + a CASA security assessment before > 100 users / non-test users. Prefer narrower scopes if they work.");
  else if (worst === "sensitive") console.log("   Sensitive scopes need Google verification (a demo video + justification) before the unverified-app warning goes away.");
  console.log(`4. Clients         ${base}/clients/create${q}`);
  console.log(`   Application type: Web application     Name: ${name} web`);
  for (const r of redirects) console.log(`   Authorized redirect URI: ${r}`);
  console.log("   Create, then copy the client ID and secret at once (the secret is not shown again; download the JSON).");
  console.log("5. Store, never print. e.g.");
  console.log("   fly secrets set -a <app> GOOGLE_CLIENT_ID=… GOOGLE_CLIENT_SECRET=…   |   vercel env add GOOGLE_CLIENT_SECRET production");
  console.log("6. Verify from outside:");
  console.log(`   node ${process.argv[1]} check-client --client-id <ID> --expect-name "${name}"${redirects.map((r) => ` --redirect ${r}`).join("")}`);
  console.log("   then do one real sign-in in a browser and confirm the consent screen says the right name.");
}

function pageText(html) {
  return html
    .replace(/<script[\s\S]*?<\/script>|<style[\s\S]*?<\/style>/g, " ")
    .replace(/<[^>]+>/g, " ")
    .replace(/&#39;|&#x27;/g, "'")
    .replace(/&quot;/g, '"')
    .replace(/&amp;/g, "&")
    .replace(/\s+/g, " ");
}

// What a signed-out user's browser gets for this client + redirect. Google
// validates the client and redirect before asking anyone to sign in, and the
// sign-in page names the app from the consent screen's Branding.
async function probe(clientId, redirect, scopes) {
  const url = new URL("https://accounts.google.com/o/oauth2/v2/auth");
  url.search = new URLSearchParams({ client_id: clientId, redirect_uri: redirect, response_type: "code", scope: scopes.join(" "), prompt: "select_account" }).toString();
  const response = await fetch(url, { redirect: "follow", headers: { "user-agent": "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_0) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0 Safari/537.36", "accept-language": "en-US" } });
  const html = await response.text();
  const text = pageText(html);
  const error = text.match(/Error 400: (\w+)/)?.[1] ?? text.match(/Error 401: (\w+)/)?.[1] ?? text.match(/Error 403: (\w+)/)?.[1] ?? (response.url.includes("/signin/oauth/error") ? "unknown_error" : null);
  // The name sits in a data-app-name attribute (seen 2026-10); the text
  // match is the fallback for older page shapes. Unverified branding shows
  // the redirect URI's domain here, not the App name.
  const appName = pageText(html.match(/data-app-name="([^"]+)"/)?.[1] ?? "").trim()
    || text.match(/to continue to ([^.]+?)(?:\s+(?:Email|Forgot|Not your|Create account|Next)\b|$)/)?.[1]?.trim()
    || null;
  return { error, appName, finalUrl: response.url };
}

async function checkClient(o) {
  const clientId = o.one("client-id");
  const redirects = o.many("redirect");
  const expect = o.one("expect-name");
  const scopes = ["openid", "email", ...o.many("scopes").map((s) => SCOPES[s]?.[0] ?? s)];
  if (!clientId || !redirects.length) fail("--client-id and at least one --redirect are required");
  if (!/\.apps\.googleusercontent\.com$/.test(clientId)) fail("that doesn't look like a Google OAuth client ID (…apps.googleusercontent.com)");
  let ok = true;
  for (const redirect of redirects) {
    const r = await probe(clientId, redirect, scopes);
    if (r.error) {
      ok = false;
      const hint = {
        redirect_uri_mismatch: `add exactly ${redirect} to the client's Authorized redirect URIs (scheme, host, path and trailing slash must match)`,
        invalid_client: "the client ID is wrong, or the client was deleted",
        deleted_client: "this client was deleted; create a new one",
        invalid_request: "the request is malformed (often a non-https redirect or a scope typo)",
        access_denied: "the app is blocked (org policy, or Testing mode without this user as a test user)",
        org_internal: "the consent screen is Internal; switch Audience to External",
      }[r.error] ?? "see the Console's client page";
      console.log(`✗ ${redirect}: ${r.error}. Fix: ${hint}.`);
      continue;
    }
    if (!r.appName) {
      console.log(`? ${redirect}: accepted, but the app name wasn't readable (Google changed the page?). Check by signing in once.`);
      continue;
    }
    const nameOk = !expect || r.appName.toLowerCase() === expect.toLowerCase();
    if (!nameOk) ok = false;
    console.log(`${nameOk ? "✓" : "✗"} ${redirect}: accepted; Google shows "continue to ${r.appName}"${nameOk ? "" : `, expected "${expect}". ${/\./.test(r.appName) ? "Google shows a domain until the branding is verified (Branding page, Verify branding)." : "Fix the App name on the Branding page (it is not the project name)."}`}`);
  }
  if (!ok) process.exit(1);
}

// ---------------------------------------------------------------------------

const o = parseArgs(process.argv.slice(2));
try {
  switch (o.command) {
    case "whoami": await whoami(); break;
    case "projects": await projects(o.one("filter")); break;
    case "ensure-project": await ensureProject(o.one("id"), o.one("name"), o.one("org"), o.one("folder")); break;
    case "enable": await enable(o.one("project"), o.many("apis")); break;
    case "audit": await audit(o.one("project")); break;
    case "oauth-plan": oauthPlan(o); break;
    case "check-client": await checkClient(o); break;
    default:
      console.log(readFileSync(new URL(import.meta.url), "utf8").split("\n").slice(1, 19).map((l) => l.replace(/^\/\/ ?/, "")).join("\n"));
      process.exit(o.command ? 1 : 0);
  }
} catch (error) {
  fail(error.message);
}
