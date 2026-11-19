#!/usr/bin/env node
// Writes and verifies appcast.xml, the Sparkle feed the Mac app checks
// (SUFeedURL in App/Info.plist:
// https://github.com/exla-ai/rudder/releases/latest/download/appcast.xml).
// It has one item, the release that carries it: Rudder.dmg on GitHub release
// mac-v<version>-<build>, with its length and EdDSA signature.
// .github/workflows/mac-publish.yml runs both.
//
//   node mac/scripts/appcast.mjs write --dmg <Rudder.dmg> --version 5.0.8 --build 5000008 \
//     --minimum-system 14.0 --info-plist <Info.plist> --key <private key file> --out <appcast.xml>
//   node mac/scripts/appcast.mjs verify --appcast <appcast.xml> --dmg <Rudder.dmg> --version 5.0.8 \
//     --build 5000008 --minimum-system 14.0 --info-plist <Info.plist>
//
// The key file is what Sparkle's `generate_keys -x` exports: base64 of the
// 32-byte Ed25519 seed. The signature is Ed25519 over the DMG's bytes, base64,
// as Sparkle's sign_update prints it. --info-plist is the App/Info.plist of the
// commit the DMG was built from: its SUPublicEDKey is the key the installed
// apps check, and they refuse an update signed with any other. `write` refuses
// a key that is not that one. `verify` reads only the public key: it checks the
// written feed's signature against the DMG and that its version, build,
// minimum macOS, URL and length are the release's.
import crypto from "node:crypto"
import fs from "node:fs"
import { parseArgs } from "node:util"

const REPO = "exla-ai/rudder"
// DER prefixes for an Ed25519 key: PKCS#8 before the 32-byte seed, SPKI before
// the 32-byte public key.
const PKCS8_ED25519 = Buffer.from("302e020100300506032b657004220420", "hex")
const SPKI_ED25519 = Buffer.from("302a300506032b6570032100", "hex")

function fail(message) {
  console.error(`appcast.mjs: ${message}`)
  process.exit(1)
}

const [command, ...rest] = process.argv.slice(2)
const required = {
  write: ["dmg", "version", "build", "minimum-system", "info-plist", "key", "out"],
  verify: ["appcast", "dmg", "version", "build", "minimum-system", "info-plist"],
}[command]
if (!required) fail("usage: appcast.mjs write|verify <options> (see the top of this file)")
const { values } = parseArgs({
  args: rest,
  options: Object.fromEntries(required.map((name) => [name, { type: "string" }])),
  strict: true,
})
for (const name of required) {
  if (!values[name]) fail(`--${name} is required`)
}
const { dmg, version, build } = values
const minimumSystem = values["minimum-system"]
if (!/^\d+\.\d+\.\d+$/.test(version)) fail(`--version must look like 5.0.8, got ${version}`)
// Sparkle compares sparkle:version with the installed CFBundleVersion.
if (!/^\d+$/.test(build)) fail(`--build must be the CFBundleVersion number, got ${build}`)
if (!/^\d+\.\d+(\.\d+)?$/.test(minimumSystem)) fail(`--minimum-system must look like 14.0, got ${minimumSystem}`)

const infoPlist = fs.readFileSync(values["info-plist"], "utf8")
const shipped = infoPlist.match(/<key>SUPublicEDKey<\/key>\s*<string>([^<]+)<\/string>/)?.[1]
if (!shipped) fail(`${values["info-plist"]} has no SUPublicEDKey`)
const shippedRaw = Buffer.from(shipped, "base64")
if (shippedRaw.length !== 32) fail(`SUPublicEDKey in ${values["info-plist"]} is not base64 of a 32-byte Ed25519 key`)
const shippedKey = crypto.createPublicKey({ key: Buffer.concat([SPKI_ED25519, shippedRaw]), format: "der", type: "spki" })

const archive = fs.readFileSync(dmg)
const url = `https://github.com/${REPO}/releases/download/mac-v${version}-${build}/Rudder.dmg`

if (command === "write") {
  const seed = Buffer.from(fs.readFileSync(values.key, "utf8").trim(), "base64")
  if (seed.length !== 32) fail(`${values.key} must hold base64 of a 32-byte Ed25519 seed (Sparkle's generate_keys -x format)`)
  const privateKey = crypto.createPrivateKey({ key: Buffer.concat([PKCS8_ED25519, seed]), format: "der", type: "pkcs8" })
  const publicKey = crypto.createPublicKey(privateKey).export({ format: "der", type: "spki" }).subarray(-32)
  if (!publicKey.equals(shippedRaw)) fail(`${values.key} is not the key for SUPublicEDKey in ${values["info-plist"]}`)

  const signature = crypto.sign(null, archive, privateKey)
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
  fs.writeFileSync(values.out, xml)
  console.log(`appcast.mjs: wrote ${values.out} for Rudder ${version} (${build}), ${archive.length} bytes at ${url}`)
} else {
  const feed = fs.readFileSync(values.appcast, "utf8")
  const items = feed.match(/<item>[\s\S]*?<\/item>/g) ?? []
  if (items.length !== 1) fail(`${values.appcast} must have exactly one <item>, has ${items.length}`)
  const item = items[0]
  const element = (name) => item.match(new RegExp(`<${name}>([^<]*)</${name}>`))?.[1]
  const enclosure = item.match(/<enclosure\s[^>]*\/>/)?.[0]
  if (!enclosure) fail(`${values.appcast} has no <enclosure>`)
  const attribute = (name) => enclosure.match(new RegExp(`\\s${name}="([^"]*)"`))?.[1]
  const expected = {
    "sparkle:version": [element("sparkle:version"), build],
    "sparkle:shortVersionString": [element("sparkle:shortVersionString"), version],
    "sparkle:minimumSystemVersion": [element("sparkle:minimumSystemVersion"), minimumSystem],
    "enclosure url": [attribute("url"), url],
    "enclosure length": [attribute("length"), String(archive.length)],
  }
  for (const [name, [found, want]] of Object.entries(expected)) {
    if (found !== want) fail(`${values.appcast}: ${name} is ${found ?? "missing"}, the release has ${want}`)
  }
  const signature = attribute("sparkle:edSignature")
  if (!signature) fail(`${values.appcast} has no sparkle:edSignature`)
  if (!crypto.verify(null, archive, shippedKey, Buffer.from(signature, "base64"))) {
    fail(`the signature in ${values.appcast} does not verify against SUPublicEDKey in ${values["info-plist"]}: installed apps would refuse this update`)
  }
  console.log(`appcast.mjs: ${values.appcast} verifies against SUPublicEDKey ${shipped} for Rudder ${version} (${build})`)
}
