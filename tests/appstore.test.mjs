// ios/scripts/appstore.mjs: which App Store version prepare touches, the
// screenshot checks that stand in for App Store Connect's late refusals, and
// the team and bundle id read from ios/project.yml.
import assert from "node:assert/strict"
import { execFileSync } from "node:child_process"
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs"
import { tmpdir } from "node:os"
import path from "node:path"
import test from "node:test"
import { appIdentity, checkListing, ensureBundleId, imageInfo, planVersion } from "../ios/scripts/appstore.mjs"

const v = (versionString, appVersionState) => ({ id: versionString, attributes: { platform: "IOS", versionString, appVersionState } })

test("planVersion uses, renames or creates, and refuses a version it cannot change", () => {
  assert.equal(planVersion([], "5.1.0").action, "create")
  assert.equal(planVersion([v("5.0.0", "READY_FOR_DISTRIBUTION")], "5.1.0").action, "create")
  assert.equal(planVersion([v("5.1.0", "REJECTED")], "5.1.0").action, "use")
  const rename = planVersion([v("5.0.0", "READY_FOR_DISTRIBUTION"), v("1.0", "PREPARE_FOR_SUBMISSION")], "5.1.0")
  assert.deepEqual([rename.action, rename.version.id], ["rename", "1.0"])
  assert.throws(() => planVersion([v("5.1.0", "READY_FOR_DISTRIBUTION")], "5.1.0"), /takes no changes/)
  assert.throws(() => planVersion([v("5.0.9", "WAITING_FOR_REVIEW")], "5.1.0"), /5\.0\.9 is WAITING_FOR_REVIEW/)
})

function png(width, height, colorType, chunks = []) {
  const header = Buffer.alloc(33)
  header.writeUInt32BE(0x89504e47, 0)
  header.writeUInt32BE(0x0d0a1a0a, 4)
  header.writeUInt32BE(13, 8)
  header.write("IHDR", 12, "latin1")
  header.writeUInt32BE(width, 16)
  header.writeUInt32BE(height, 20)
  header[24] = 8
  header[25] = colorType
  const chunk = (type) => {
    const bytes = Buffer.alloc(12)
    bytes.write(type, 4, "latin1")
    return bytes
  }
  return Buffer.concat([header, ...chunks.map(chunk), chunk("IDAT")])
}

function jpeg(width, height) {
  const app0 = Buffer.from([0xff, 0xe0, 0x00, 0x04, 0x00, 0x00])
  const sof = Buffer.from([0xff, 0xc0, 0x00, 0x11, 0x08, height >> 8, height & 255, width >> 8, width & 255, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0])
  return Buffer.concat([Buffer.from([0xff, 0xd8]), app0, sof, Buffer.from([0xff, 0xd9])])
}

test("imageInfo reads sizes and finds alpha", () => {
  assert.deepEqual(imageInfo(png(1320, 2868, 2)), { format: "png", width: 1320, height: 2868, alpha: false })
  assert.equal(imageInfo(png(1320, 2868, 6)).alpha, true)
  assert.equal(imageInfo(png(1320, 2868, 2, ["tRNS"])).alpha, true)
  assert.deepEqual(imageInfo(jpeg(2064, 2752)), { format: "jpeg", width: 2064, height: 2752, alpha: false })
})

test("checkListing names wrong sizes and alpha, and hashes good sets in name order", () => {
  const dir = mkdtempSync(path.join(tmpdir(), "appstore-test-"))
  try {
    const iphone = path.join(dir, "screenshots", "APP_IPHONE_67")
    mkdirSync(iphone, { recursive: true })
    writeFileSync(path.join(iphone, "02-chat.jpg"), jpeg(1320, 2868))
    writeFileSync(path.join(iphone, "01-home.jpg"), jpeg(1320, 2868))
    const listing = {
      locale: "en-US", name: "Rudder", subtitle: "Steer Agents from your Phone", primaryCategory: "DEVELOPER_TOOLS",
      secondaryCategory: "PRODUCTIVITY", contentRightsDeclaration: "DOES_NOT_USE_THIRD_PARTY_CONTENT", copyright: "2026 Exla",
      releaseType: "MANUAL", keywords: "ai,coding", promotionalText: "", supportUrl: "https://example.com/support",
      marketingUrl: "https://example.com", privacyPolicyUrl: "https://example.com/privacy", screenshotSets: ["APP_IPHONE_67"],
      description: "Rudder steers agents.", whatsNew: "New.", reviewNotes: "Sign in with the demo account.",
    }
    const sets = checkListing(listing, dir)
    assert.deepEqual(sets[0].files.map((file) => file.name), ["01-home.jpg", "02-chat.jpg"])
    assert.match(sets[0].files[0].md5, /^[0-9a-f]{32}$/)

    writeFileSync(path.join(iphone, "03-settings.png"), png(1290, 2796, 6))
    writeFileSync(path.join(iphone, "04-ipad.jpg"), jpeg(2064, 2752))
    assert.throws(() => checkListing({ ...listing, subtitle: "Steer agents — from anywhere at all" }, dir), (error) => {
      assert.match(error.message, /subtitle is 35 characters/)
      assert.match(error.message, /subtitle has an em dash/)
      assert.match(error.message, /03-settings\.png has an alpha channel/)
      assert.match(error.message, /04-ipad\.jpg is 2064x2752/)
      return true
    })
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
})

test("appIdentity and identity.sh read the same team and bundle id, and entitlements come from the target's file", () => {
  const real = appIdentity()
  const shell = execFileSync("bash", ["-c", 'source ios/scripts/identity.sh && echo "$TEAM_ID $BUNDLE_ID"'], { encoding: "utf8" }).trim()
  assert.equal(shell, `${real.team} ${real.bundleId}`)
  assert.deepEqual(real.entitlements, [])

  const root = mkdtempSync(path.join(tmpdir(), "appstore-identity-"))
  try {
    mkdirSync(path.join(root, "ios/App"), { recursive: true })
    writeFileSync(path.join(root, "ios/project.yml"), [
      "settings:", "  base:", "    DEVELOPMENT_TEAM: TEAM123", "targets:", "  Rudder:", "    settings:", "      base:",
      "        PRODUCT_BUNDLE_IDENTIFIER: ai.example.app", "", "        CODE_SIGN_ENTITLEMENTS: App/Rudder.entitlements",
      "  # tests", "  RudderUITests:", "    settings:", "      base:", "        PRODUCT_BUNDLE_IDENTIFIER: ai.example.app.uitests", "",
    ].join("\n"))
    writeFileSync(path.join(root, "ios/App/Rudder.entitlements"),
      "<plist><dict><key>aps-environment</key><string>production</string><key>com.apple.developer.associated-domains</key><array/></dict></plist>")
    assert.deepEqual(appIdentity(root), { team: "TEAM123", bundleId: "ai.example.app", entitlements: ["aps-environment", "com.apple.developer.associated-domains"] })
  } finally {
    rmSync(root, { recursive: true, force: true })
  }
})

// Answers GETs from `data` by the URL's last path segment and records writes.
function fakeClient(data) {
  const writes = []
  return {
    writes,
    async api(method, url, body) {
      if (method !== "GET") {
        writes.push([method, url, body?.data?.attributes])
        return { data: { type: body.data.type, id: "new", attributes: body.data.attributes } }
      }
      return { data: data[url.split("?")[0].split("/").at(-1)] }
    },
  }
}

test("ensureBundleId enables mapped capabilities once, skips CarPlay and refuses another team's key or an unmapped entitlement", async () => {
  const bundle = { type: "bundleIds", id: "B1", attributes: { identifier: "ai.example.app", seedId: "TEAM123" } }
  const context = { team: "TEAM123", bundleId: "ai.example.app", entitlements: ["aps-environment", "com.apple.developer.carplay-voice-based-conversation", "keychain-access-groups"] }
  const push = fakeClient({ bundleIds: [bundle], bundleIdCapabilities: [] })
  await ensureBundleId(push, context)
  assert.deepEqual(push.writes, [["POST", "/v1/bundleIdCapabilities", { capabilityType: "PUSH_NOTIFICATIONS" }]])
  const already = fakeClient({ bundleIds: [bundle], bundleIdCapabilities: [{ attributes: { capabilityType: "PUSH_NOTIFICATIONS" } }] })
  await ensureBundleId(already, context)
  assert.deepEqual(already.writes, [])
  const missing = fakeClient({ bundleIds: [], bundleIdCapabilities: [] })
  await assert.rejects(ensureBundleId(missing, { ...context, entitlements: [] }), /App ID prefix undefined, not TEAM123/)
  assert.equal(missing.writes[0][1], "/v1/bundleIds")
  await assert.rejects(ensureBundleId(fakeClient({ bundleIds: [bundle], bundleIdCapabilities: [] }), { ...context, team: "OTHER" }), /belongs to another team/)
  await assert.rejects(ensureBundleId(fakeClient({ bundleIds: [bundle], bundleIdCapabilities: [] }), { ...context, entitlements: ["com.apple.developer.healthkit"] }), /no capability is mapped/)
})
