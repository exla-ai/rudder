#!/usr/bin/env node
// After an upload: wait for App Store Connect to finish processing the build,
// add it to the external "Beta testers" group (the public TestFlight link and
// invited testers) and submit it for beta review. Internal testers ("Team",
// access to all builds) get it as soon as it is processed.
//
// Env: ASC_KEY_ID, ASC_ISSUER_ID, ASC_KEY_PATH (the .p8), BUILD_NUMBER.
// Optional: APP_IDENTIFIER (default dev.viraat.rudder), EXTERNAL_GROUP.
import { createPrivateKey, sign } from "node:crypto";
import { readFileSync } from "node:fs";

const { ASC_KEY_ID, ASC_ISSUER_ID, ASC_KEY_PATH, BUILD_NUMBER } = process.env;
const bundleId = process.env.APP_IDENTIFIER || "dev.viraat.rudder";
const groupName = process.env.EXTERNAL_GROUP || "Beta testers";
if (!ASC_KEY_ID || !ASC_ISSUER_ID || !ASC_KEY_PATH || !BUILD_NUMBER) {
  console.error("ASC_KEY_ID, ASC_ISSUER_ID, ASC_KEY_PATH and BUILD_NUMBER are required");
  process.exit(2);
}
const key = createPrivateKey(readFileSync(ASC_KEY_PATH, "utf8"));

function token() {
  const b64 = (value) => Buffer.from(JSON.stringify(value)).toString("base64url");
  const now = Math.floor(Date.now() / 1000);
  const body = `${b64({ alg: "ES256", kid: ASC_KEY_ID, typ: "JWT" })}.${b64({ iss: ASC_ISSUER_ID, iat: now, exp: now + 1200, aud: "appstoreconnect-v1" })}`;
  const signature = sign("sha256", Buffer.from(body), { key, dsaEncoding: "ieee-p1363" }).toString("base64url");
  return `${body}.${signature}`;
}

async function api(method, path, body) {
  const response = await fetch(`https://api.appstoreconnect.apple.com/v1/${path}`, {
    method,
    headers: { Authorization: `Bearer ${token()}`, "Content-Type": "application/json" },
    body: body ? JSON.stringify(body) : undefined,
  });
  const text = await response.text();
  if (!response.ok) {
    throw new Error(`${method} ${path}: ${response.status} ${text.slice(0, 400)}`);
  }
  return text ? JSON.parse(text) : {};
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

const app = (await api("GET", `apps?filter[bundleId]=${encodeURIComponent(bundleId)}`)).data[0];
if (!app) {
  throw new Error(`no app record for ${bundleId}`);
}

// Processing usually takes 5-15 minutes; the build is not listed at all at first.
let build;
for (let attempt = 0; attempt < 60; attempt += 1) {
  build = (await api("GET", `builds?filter[app]=${app.id}&filter[version]=${encodeURIComponent(BUILD_NUMBER)}&limit=1`)).data[0];
  const state = build?.attributes?.processingState;
  console.log(`build ${BUILD_NUMBER}: ${state ?? "not listed yet"}`);
  if (state === "VALID") {
    break;
  }
  if (state === "FAILED" || state === "INVALID") {
    throw new Error(`build ${BUILD_NUMBER} processing ${state}`);
  }
  await sleep(30_000);
}
if (build?.attributes?.processingState !== "VALID") {
  throw new Error(`build ${BUILD_NUMBER} did not finish processing in 30 minutes`);
}

const group = (await api("GET", `betaGroups?filter[app]=${app.id}&filter[name]=${encodeURIComponent(groupName)}`)).data[0];
if (!group) {
  throw new Error(`no beta group named ${groupName}`);
}
await api("POST", `betaGroups/${group.id}/relationships/builds`, { data: [{ type: "builds", id: build.id }] });
console.log(`added build ${BUILD_NUMBER} to ${groupName}`);

try {
  await api("POST", "betaAppReviewSubmissions", { data: { type: "betaAppReviewSubmissions", relationships: { build: { data: { type: "builds", id: build.id } } } } });
  console.log(`submitted build ${BUILD_NUMBER} for beta review`);
} catch (error) {
  // Already submitted (a rerun), or Apple reviews only one build at a time.
  console.log(`beta review: ${error.message}`);
}
