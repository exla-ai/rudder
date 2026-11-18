#!/usr/bin/env node
// Writes appcast.xml, the Sparkle feed the Mac app checks (SUFeedURL in
// App/Info.plist: https://github.com/exla-ai/rudder/releases/latest/download/appcast.xml).
// It has one item, the release that carries it: Rudder.dmg on GitHub release
// mac-v<version>-<build>, with its length and EdDSA signature.
//
//   node mac/scripts/appcast.mjs --dmg <Rudder.dmg> --version 1.2.0 --build 5 \
//     --minimum-system 14.0 --key <private key file> --out <appcast.xml>
//
// The key file is what Sparkle's `generate_keys -x` exports: base64 of the
// 32-byte Ed25519 seed. The signature is Ed25519 over the DMG's bytes, base64,
// as Sparkle's sign_update prints it. The key must match SUPublicEDKey in
// App/Info.plist, or every installed app would refuse the update; the script
// checks that and verifies the signature before writing.
import crypto from "node:crypto"
import fs from "node:fs"
import { parseArgs } from "node:util"

const REPO = "exla-ai/rudder"
// PKCS#8 DER for an Ed25519 private key is this prefix and the 32-byte seed.
const PKCS8_ED25519 = Buffer.from("302e020100300506032b657004220420", "hex")

function fail(message) {
  console.error(`appcast.mjs: ${message}`)
  process.exit(1)
}

const { values } = parseArgs({
  options: {
    dmg: { type: "string" },
    version: { type: "string" },
    build: { type: "string" },
    "minimum-system": { type: "string" },
    key: { type: "string" },
    out: { type: "string" },
  },
  strict: true,
})
for (const name of ["dmg", "version", "build", "minimum-system", "key", "out"]) {
  if (!values[name]) fail(`--${name} is required`)
}
const { dmg, version, build, key, out } = values
const minimumSystem = values["minimum-system"]
if (!/^\d+\.\d+\.\d+$/.test(version)) fail(`--version must look like 1.2.0, got ${version}`)
// Sparkle compares sparkle:version with the installed CFBundleVersion.
if (!/^\d+$/.test(build)) fail(`--build must be the CFBundleVersion number, got ${build}`)
if (!/^\d+\.\d+(\.\d+)?$/.test(minimumSystem)) fail(`--minimum-system must look like 14.0, got ${minimumSystem}`)

const seed = Buffer.from(fs.readFileSync(key, "utf8").trim(), "base64")
if (seed.length !== 32) fail(`${key} must hold base64 of a 32-byte Ed25519 seed (Sparkle's generate_keys -x format)`)
const privateKey = crypto.createPrivateKey({ key: Buffer.concat([PKCS8_ED25519, seed]), format: "der", type: "pkcs8" })
const publicKey = crypto.createPublicKey(privateKey)
const publicKeyBase64 = publicKey.export({ format: "der", type: "spki" }).subarray(-32).toString("base64")

const infoPlist = fs.readFileSync(new URL("../App/Info.plist", import.meta.url), "utf8")
const shipped = infoPlist.match(/<key>SUPublicEDKey<\/key>\s*<string>([^<]+)<\/string>/)?.[1]
if (!shipped) fail("App/Info.plist has no SUPublicEDKey")
if (shipped !== publicKeyBase64) fail(`${key} is not the key for SUPublicEDKey in App/Info.plist`)

const archive = fs.readFileSync(dmg)
const signature = crypto.sign(null, archive, privateKey)
if (!crypto.verify(null, archive, publicKey, signature)) fail("the signature does not verify")

const tag = `mac-v${version}-${build}`
const url = `https://github.com/${REPO}/releases/download/${tag}/Rudder.dmg`
const xml = `<?xml version="1.0" encoding="utf-8"?>
<rss version="2.0" xmlns:sparkle="http://www.andymatuschak.org/xml-namespaces/sparkle">
  <channel>
    <title>Rudder for Mac</title>
    <link>https://rudder.exla.ai</link>
    <description>Updates to Rudder for macOS.</description>
    <language>en</language>
    <item>
      <title>Rudder ${version}</title>
      <pubDate>${new Date().toUTCString()}</pubDate>
      <sparkle:version>${build}</sparkle:version>
      <sparkle:shortVersionString>${version}</sparkle:shortVersionString>
      <sparkle:minimumSystemVersion>${minimumSystem}</sparkle:minimumSystemVersion>
      <enclosure url="${url}" length="${archive.length}" type="application/octet-stream" sparkle:edSignature="${signature.toString("base64")}"/>
    </item>
  </channel>
</rss>
`
fs.writeFileSync(out, xml)
console.log(`appcast.mjs: wrote ${out} for Rudder ${version} (${build}), ${archive.length} bytes at ${url}`)
