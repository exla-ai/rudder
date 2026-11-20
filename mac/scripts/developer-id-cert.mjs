#!/usr/bin/env node
// Issues a Developer ID Application certificate for a signing request, so the
// Mac release in CI (mac.yml, mac/scripts/ci.sh sign) can sign without a
// laptop's keychain. The private key never comes here: whoever made the CSR
// keeps it, joins it with the certificate this writes into a .p12 and stores
// that as the MAC_DEVELOPER_ID_P12 secret. Run by .github/workflows/mac-cert.yml.
//
// Env: ASC_KEY_ID, ASC_ISSUER_ID, ASC_KEY_P8 (the .p8 file's text), CSR (base64
// of the PEM signing request). Writes developer-id.cer (DER) and lists the
// team's Developer ID certificates first.
import { createPrivateKey, sign } from "node:crypto";
import { writeFileSync } from "node:fs";

const { ASC_KEY_ID, ASC_ISSUER_ID, ASC_KEY_P8, CSR } = process.env;
if (!ASC_KEY_ID || !ASC_ISSUER_ID || !ASC_KEY_P8 || !CSR) {
  console.error("ASC_KEY_ID, ASC_ISSUER_ID, ASC_KEY_P8 and CSR are required");
  process.exit(2);
}
const key = createPrivateKey(ASC_KEY_P8);

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
  if (!response.ok) throw new Error(`${method} ${path}: ${response.status} ${text.slice(0, 600)}`);
  return text ? JSON.parse(text) : {};
}

const TYPE = "DEVELOPER_ID_APPLICATION_G2";
const existing = await api("GET", `certificates?filter[certificateType]=DEVELOPER_ID_APPLICATION,${TYPE}&limit=50`);
console.log(`Developer ID Application certificates on the team: ${existing.data.length}`);
for (const cert of existing.data) {
  console.log(`  ${cert.id} ${cert.attributes.certificateType} ${cert.attributes.name} expires ${cert.attributes.expirationDate}`);
}

// The PEM's body, as the API wants it.
const csrContent = Buffer.from(CSR, "base64").toString("utf8").replace(/-----[^-]+-----/g, "").replace(/\s+/g, "");
const created = await api("POST", "certificates", {
  data: { type: "certificates", attributes: { certificateType: TYPE, csrContent } },
});
const attrs = created.data.attributes;
writeFileSync("developer-id.cer", Buffer.from(attrs.certificateContent, "base64"));
console.log(`Created ${created.data.id}: ${attrs.name}, serial ${attrs.serialNumber}, expires ${attrs.expirationDate}`);
