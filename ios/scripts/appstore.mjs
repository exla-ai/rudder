#!/usr/bin/env node
// ios/scripts/appstore.mjs: the iPhone app in App Store Connect, through its
// API. The team and bundle id come from ios/project.yml, the version is
// Rudder's (package.json, written into ios/project.yml by scripts/version.mjs)
// and the build number is derived from it. The listing comes from ios/appstore/.
//
//   check                    checks ios/appstore against Apple's limits (no network, no key)
//   status                   the app's App Store versions, latest builds and review submissions
//   identity                 checks the key belongs to the project's team, registers the
//                            bundle id if it is missing and enables the capabilities the
//                            app's entitlements need (none today)
//   testflight               waits for this build to finish processing; internal testers
//                            get it from TestFlight then
//   prepare                  makes the App Store version match the repo: creates or renames
//                            it, sets the text, categories and App Review details, attaches
//                            this build and uploads the screenshots. Submits nothing.
//   submit --confirm X.Y.Z   submits the prepared version for App Review. The only command
//                            that does, and only when --confirm names the version.
//   release --confirm X.Y.Z  releases an approved version (the listing's releaseType is MANUAL)
//
//   --dry-run  prints every request instead of sending it, answered with made-up ids
//              and states, so it needs no key.
//
// Env: IOS_ASC_KEY_ID, IOS_ASC_ISSUER_ID and IOS_ASC_KEY_P8 (the .p8 file's
// text), an App Store Connect key of the team in ios/project.yml; the Mac's
// ASC_* key is another team's. prepare also needs APPSTORE_REVIEW, JSON with the App Review contact and demo account:
// contactFirstName, contactLastName, contactPhone, contactEmail,
// demoAccountRequired (true or false), demoAccountName, demoAccountPassword.
//
// The repository and its workflow logs are public: this prints versions,
// states and counts, never the review details or anything from an account.
import { createHash, createPrivateKey, sign } from "node:crypto"
import { existsSync, readFileSync, readdirSync } from "node:fs"
import path from "node:path"
import { fileURLToPath } from "node:url"
import { appBuild, checkVersionFiles } from "../../scripts/version.mjs"

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..")
const LISTING_DIR = path.join(ROOT, "ios/appstore")
const API = "https://api.appstoreconnect.apple.com"
const KEY_ENV = ["IOS_ASC_KEY_ID", "IOS_ASC_ISSUER_ID", "IOS_ASC_KEY_P8"]

// Entitlement -> the bundle id capability App Store Connect must have enabled
// for a provisioning profile to carry it. Entitlements Apple grants on
// request (CarPlay) cannot be enabled through the API.
const ENTITLEMENT_CAPABILITIES = {
  "aps-environment": { capabilityType: "PUSH_NOTIFICATIONS" },
  "com.apple.developer.associated-domains": { capabilityType: "ASSOCIATED_DOMAINS" },
  "com.apple.security.application-groups": { capabilityType: "APP_GROUPS" },
  "com.apple.developer.applesignin": {
    capabilityType: "APPLE_ID_AUTH",
    settings: [{ key: "APPLE_ID_AUTH_APP_CONSENT", options: [{ key: "PRIMARY_APP_CONSENT" }] }],
  },
}
const NO_CAPABILITY = new Set(["keychain-access-groups"])
const GRANTED_BY_APPLE = /^com\.apple\.developer\.carplay-/

// AppVersionState values. A version takes changes only in EDITABLE; while one
// is IN_FLIGHT, App Store Connect makes no new version.
const EDITABLE = new Set(["PREPARE_FOR_SUBMISSION", "READY_FOR_REVIEW", "DEVELOPER_REJECTED", "REJECTED", "METADATA_REJECTED", "INVALID_BINARY"])
const IN_FLIGHT = new Set(["WAITING_FOR_REVIEW", "IN_REVIEW", "WAITING_FOR_EXPORT_COMPLIANCE", "ACCEPTED", "PENDING_DEVELOPER_RELEASE", "PENDING_APPLE_RELEASE", "PROCESSING_FOR_DISTRIBUTION"])
const RELEASED = new Set(["READY_FOR_DISTRIBUTION", "REPLACED_WITH_NEW_VERSION"])
const EDITABLE_APP_INFO = new Set(["PREPARE_FOR_SUBMISSION", "READY_FOR_REVIEW", "DEVELOPER_REJECTED", "REJECTED"])

// Character limits App Store Connect enforces.
const LIMITS = {
  name: [2, 30],
  subtitle: [0, 30],
  keywords: [1, 100],
  promotionalText: [0, 170],
  description: [10, 4000],
  whatsNew: [1, 4000],
  reviewNotes: [1, 4000],
}

// The pixel sizes each screenshot set takes, from App Store Connect's
// screenshot specifications. The API's ScreenshotDisplayType has no 6.9 inch
// or 13 inch name: APP_IPHONE_67 is the large Dynamic Island iPhone set (6.9
// inch), APP_IPHONE_61 the medium one (6.3 inch) and APP_IPAD_PRO_3GEN_129 the
// 13 inch iPad set. Images can't have an alpha channel.
export const SCREENSHOT_SIZES = {
  APP_IPHONE_67: [[1260, 2736], [1290, 2796], [1320, 2868]],
  APP_IPHONE_61: [[1179, 2556], [1206, 2622]],
  APP_IPAD_PRO_3GEN_129: [[2064, 2752], [2048, 2732], [2752, 2064], [2732, 2048]],
}

const REVIEW_KEYS = ["contactFirstName", "contactLastName", "contactPhone", "contactEmail", "demoAccountRequired", "demoAccountName", "demoAccountPassword"]
const LISTING_KEYS = ["locale", "name", "subtitle", "primaryCategory", "secondaryCategory", "contentRightsDeclaration", "copyright", "releaseType", "keywords", "promotionalText", "supportUrl", "marketingUrl", "privacyPolicyUrl", "screenshotSets"]

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms))

/**
 * The Apple team, bundle id and entitlement keys of the Rudder target in
 * ios/project.yml (ios/scripts/identity.sh reads the same lines).
 */
export function appIdentity(root = ROOT) {
  const text = readFileSync(path.join(root, "ios/project.yml"), "utf8")
  const team = /^    DEVELOPMENT_TEAM: (\S+)$/m.exec(text)?.[1]
  const target = /^  Rudder:\n((?:(?: {3,}.*)?\n)*)/m.exec(text)?.[1] ?? ""
  const bundleId = /PRODUCT_BUNDLE_IDENTIFIER: (\S+)/.exec(target)?.[1]
  if (!team || !bundleId) throw new Error("ios/project.yml has no DEVELOPMENT_TEAM or no PRODUCT_BUNDLE_IDENTIFIER for the Rudder target")
  const entitlementsFile = /CODE_SIGN_ENTITLEMENTS: (\S+)/.exec(target)?.[1]
  const entitlements = entitlementsFile
    ? [...readFileSync(path.join(root, "ios", entitlementsFile), "utf8").matchAll(/<key>([^<]+)<\/key>/g)].map((match) => match[1])
    : []
  return { team, bundleId, entitlements }
}

/** The version and build this checkout ships: Rudder's, in every versioned file. */
export function shippedVersion(root = ROOT) {
  const { version } = JSON.parse(readFileSync(path.join(root, "package.json"), "utf8"))
  checkVersionFiles(root, version)
  return { version, build: appBuild(version) }
}

/** Width, height and alpha of a PNG or JPEG, read from its header. */
export function imageInfo(bytes) {
  if (bytes.length > 33 && bytes.readUInt32BE(0) === 0x89504e47) {
    const colorType = bytes[25]
    let alpha = colorType === 4 || colorType === 6
    // A tRNS chunk before the image data makes some pixels transparent.
    for (let at = 8; at + 8 <= bytes.length;) {
      const length = bytes.readUInt32BE(at)
      const type = bytes.toString("latin1", at + 4, at + 8)
      if (type === "tRNS") alpha = true
      if (type === "IDAT") break
      at += 12 + length
    }
    return { format: "png", width: bytes.readUInt32BE(16), height: bytes.readUInt32BE(20), alpha }
  }
  if (bytes[0] === 0xff && bytes[1] === 0xd8) {
    for (let at = 2; at + 9 < bytes.length;) {
      if (bytes[at] !== 0xff) throw new Error("broken JPEG marker")
      const marker = bytes[at + 1]
      if (marker === 0xff) { at += 1; continue }
      if (marker >= 0xc0 && marker <= 0xcf && ![0xc4, 0xc8, 0xcc].includes(marker)) {
        return { format: "jpeg", width: bytes.readUInt16BE(at + 7), height: bytes.readUInt16BE(at + 5), alpha: false }
      }
      at += 2 + bytes.readUInt16BE(at + 2)
    }
    throw new Error("JPEG without a frame header")
  }
  throw new Error("neither PNG nor JPEG")
}

export function loadListing(dir = LISTING_DIR) {
  const listing = JSON.parse(readFileSync(path.join(dir, "listing.json"), "utf8"))
  const text = (name) => readFileSync(path.join(dir, name), "utf8").trim()
  return { ...listing, description: text("description.txt"), whatsNew: text("whats-new.txt"), reviewNotes: text("review-notes.txt") }
}

/**
 * Throws with every problem in the listing at once. Returns the screenshot
 * sets in order, each with its files (sorted by name) and their MD5s.
 */
export function checkListing(listing, dir = LISTING_DIR) {
  const problems = []
  for (const key of LISTING_KEYS) {
    if (listing[key] === undefined) problems.push(`listing.json has no ${key}`)
  }
  for (const key of Object.keys(listing)) {
    if (![...LISTING_KEYS, "description", "whatsNew", "reviewNotes"].includes(key)) problems.push(`listing.json has an unknown key ${key}`)
  }
  for (const [key, [min, max]] of Object.entries(LIMITS)) {
    const value = listing[key] ?? ""
    if ([...value].length < min || [...value].length > max) problems.push(`${key} is ${[...value].length} characters; App Store Connect takes ${min} to ${max}`)
    if (value.includes("—")) problems.push(`${key} has an em dash`)
  }
  for (const key of ["supportUrl", "marketingUrl", "privacyPolicyUrl"]) {
    if (!/^https:\/\/\S+$/.test(listing[key] ?? "")) problems.push(`${key} is not an https URL`)
  }
  if (!["MANUAL", "AFTER_APPROVAL"].includes(listing.releaseType)) problems.push("releaseType is not MANUAL or AFTER_APPROVAL")
  if (/,\s/.test(listing.keywords ?? "")) problems.push("keywords has a space after a comma; it counts against the 100")

  const sets = []
  for (const type of listing.screenshotSets ?? []) {
    const sizes = SCREENSHOT_SIZES[type]
    if (!sizes) {
      problems.push(`screenshot set ${type} is not one of ${Object.keys(SCREENSHOT_SIZES).join(", ")}`)
      continue
    }
    const folder = path.join(dir, "screenshots", type)
    const names = existsSync(folder) ? readdirSync(folder).filter((name) => /\.(png|jpe?g)$/i.test(name)).sort() : []
    if (names.length < 1 || names.length > 10) {
      problems.push(`ios/appstore/screenshots/${type} has ${names.length} images; a set takes 1 to 10`)
      continue
    }
    const files = []
    for (const name of names) {
      const file = path.join(folder, name)
      const bytes = readFileSync(file)
      let info
      try {
        info = imageInfo(bytes)
      } catch (error) {
        problems.push(`${type}/${name}: ${error.message}`)
        continue
      }
      if (!sizes.some(([width, height]) => width === info.width && height === info.height)) {
        problems.push(`${type}/${name} is ${info.width}x${info.height}; the set takes ${sizes.map((size) => size.join("x")).join(", ")}`)
      }
      if (info.alpha) problems.push(`${type}/${name} has an alpha channel; save it as JPEG`)
      files.push({ path: file, name, size: bytes.length, md5: createHash("md5").update(bytes).digest("hex") })
    }
    sets.push({ type, files })
  }
  if (problems.length) throw new Error(`ios/appstore is not ready:\n  ${problems.join("\n  ")}`)
  return sets
}

/** The App Review contact and demo account, from APPSTORE_REVIEW. Never printed. */
function reviewDetails(dryRun) {
  const raw = process.env.APPSTORE_REVIEW
  if (!raw && dryRun) return Object.fromEntries(REVIEW_KEYS.map((key) => [key, "<APPSTORE_REVIEW>"]))
  if (!raw) throw new Error("APPSTORE_REVIEW is not set (JSON with the App Review contact and demo account)")
  let review
  try {
    review = JSON.parse(raw)
  } catch {
    // JSON.parse quotes the input in its message; the input is a secret.
    throw new Error("APPSTORE_REVIEW is not valid JSON")
  }
  const problems = []
  for (const key of Object.keys(review)) if (!REVIEW_KEYS.includes(key)) problems.push(`unknown key ${key}`)
  for (const key of REVIEW_KEYS) {
    const want = key === "demoAccountRequired" ? "boolean" : "string"
    if (typeof review[key] !== want) problems.push(`${key} is not a ${want}`)
  }
  if (problems.length) throw new Error(`APPSTORE_REVIEW: ${problems.join(", ")}`)
  return review
}

/**
 * What prepare does with the App Store version for `version`, given the app's
 * iOS versions: use it, rename the one open version to it, or create it.
 */
export function planVersion(list, version) {
  const state = (v) => {
    const value = v.attributes.appVersionState
    if (!value) throw new Error(`version ${v.attributes.versionString} has no appVersionState`)
    return value
  }
  const same = list.find((v) => v.attributes.versionString === version)
  if (same) {
    if (EDITABLE.has(state(same))) return { action: "use", version: same }
    throw new Error(`${version} is ${state(same)} in App Store Connect and takes no changes; ship a new Rudder version`)
  }
  const busy = list.find((v) => IN_FLIGHT.has(state(v)))
  if (busy) throw new Error(`${busy.attributes.versionString} is ${state(busy)}; App Store Connect takes a new version once it is released or rejected`)
  const open = list.find((v) => EDITABLE.has(state(v)))
  if (open) return { action: "rename", version: open }
  return { action: "create" }
}

function realClient() {
  const missing = KEY_ENV.filter((name) => !process.env[name])
  if (missing.length) throw new Error(`${missing.join(", ")} not set`)
  const key = createPrivateKey(process.env.IOS_ASC_KEY_P8)
  const token = () => {
    const b64 = (value) => Buffer.from(JSON.stringify(value)).toString("base64url")
    const now = Math.floor(Date.now() / 1000)
    const body = `${b64({ alg: "ES256", kid: process.env.IOS_ASC_KEY_ID, typ: "JWT" })}.${b64({ iss: process.env.IOS_ASC_ISSUER_ID, iat: now, exp: now + 1200, aud: "appstoreconnect-v1" })}`
    return `${body}.${sign("sha256", Buffer.from(body), { key, dsaEncoding: "ieee-p1363" }).toString("base64url")}`
  }
  return {
    dryRun: false,
    async api(method, url, body, { quiet = false } = {}) {
      const response = await fetch(`${API}${url}`, {
        method,
        headers: { Authorization: `Bearer ${token()}`, "Content-Type": "application/json" },
        body: body ? JSON.stringify(body) : undefined,
      })
      const text = await response.text()
      if (!response.ok) {
        // Apple's errors name fields and states. A quiet request's could quote what it sent.
        let detail = ""
        try {
          detail = JSON.parse(text).errors.map((e) => quiet ? e.code : `${e.code}: ${e.detail}`).join("; ")
        } catch {
          detail = quiet ? "" : text.slice(0, 300)
        }
        throw new Error(`${method} ${url.split("?")[0]}: ${response.status} ${detail}`)
      }
      return text ? JSON.parse(text) : {}
    },
    async put(operation, bytes) {
      const response = await fetch(operation.url, {
        method: operation.method,
        headers: Object.fromEntries(operation.requestHeaders.map((header) => [header.name, header.value])),
        body: bytes.subarray(operation.offset, operation.offset + operation.length),
      })
      if (!response.ok) throw new Error(`screenshot upload part at ${operation.offset}: ${response.status}`)
    },
  }
}

/** Prints each request and answers it as App Store Connect plausibly would. */
function dryClient(context) {
  const one = (type, id, attributes = {}, relationships) => ({ type, id, attributes, ...(relationships ? { relationships } : {}) })
  const versionState = { prepare: null, submit: "PREPARE_FOR_SUBMISSION", release: "PENDING_DEVELOPER_RELEASE" }[context.command]
  const answers = {
    apps: () => [one("apps", "app", { bundleId: context.bundleId, name: "Rudder" })],
    bundleIds: () => [one("bundleIds", "bundle-id", { identifier: context.bundleId, platform: "IOS", seedId: context.team })],
    bundleIdCapabilities: () => [],
    appStoreVersions: () => versionState ? [one("appStoreVersions", "version", { platform: "IOS", versionString: context.version, appVersionState: versionState })] : [],
    appStoreVersionLocalizations: () => [one("appStoreVersionLocalizations", "version-localization", { locale: "en-US" })],
    appInfos: () => [one("appInfos", "app-info", { state: "PREPARE_FOR_SUBMISSION" })],
    appInfoLocalizations: () => [one("appInfoLocalizations", "app-info-localization", { locale: "en-US" })],
    builds: () => [one("builds", "build", { version: context.build, processingState: "VALID", uploadedDate: "2026-01-01T00:00:00Z" })],
    appScreenshotSets: () => [],
    appScreenshots: () => [],
    reviewSubmissions: () => [],
    items: () => [],
  }
  const redact = (body) => JSON.parse(JSON.stringify(body, (key, value) => REVIEW_KEYS.includes(key) ? "<APPSTORE_REVIEW>" : value))
  return {
    dryRun: true,
    async api(method, url, body) {
      console.log(`  ${method} ${url}${body ? ` ${JSON.stringify(redact(body))}` : ""}`)
      if (method === "POST") {
        const { type, attributes = {} } = body.data
        const extra = type === "appScreenshots"
          ? { uploadOperations: [{ method: "PUT", url: "<upload url>", offset: 0, length: attributes.fileSize, requestHeaders: [] }] }
          : {}
        return { data: one(type, `new-${type}`, extra) }
      }
      if (method !== "GET") return {}
      const parts = url.split("?")[0].split("/").filter(Boolean)
      const [last, parent] = [parts.at(-1), parts.at(-2)]
      if (parent === "appStoreVersions") {
        const build = context.command === "prepare" ? null : { type: "builds", id: "build" }
        return { data: one("appStoreVersions", last, { versionString: context.version }, { build: { data: build }, appStoreReviewDetail: { data: null } }) }
      }
      if (parent === "appScreenshots") return { data: one("appScreenshots", last, { assetDeliveryState: { state: "COMPLETE" } }) }
      const answer = answers[last]
      if (!answer) throw new Error(`the dry run has no answer for GET ${url}`)
      return { data: answer(), included: [] }
    },
    async put(operation) {
      console.log(`  ${operation.method} ${operation.url} (${operation.length} bytes)`)
    },
  }
}

async function findApp(client, { bundleId }) {
  const app = (await client.api("GET", `/v1/apps?filter[bundleId]=${bundleId}`)).data.find((a) => a.attributes.bundleId === bundleId)
  if (!app) {
    throw new Error(`App Store Connect has no app for ${bundleId}, and the API cannot create one. In App Store Connect: Apps, +, New App; platform iOS, name Rudder, primary language English (U.S.), bundle id ${bundleId}, SKU rudder-ios, full access`)
  }
  return app
}

async function iosVersions(client, app) {
  return (await client.api("GET", `/v1/apps/${app.id}/appStoreVersions?limit=200`)).data.filter((v) => v.attributes.platform === "IOS")
}

async function findBuild(client, app, build) {
  return (await client.api("GET", `/v1/builds?filter[app]=${app.id}&filter[version]=${build}&limit=1`)).data[0]
}

const ref = (type, id) => ({ data: { type, id } })

async function upsert(client, existing, type, attributes, relationships) {
  if (existing) {
    await client.api("PATCH", `/v1/${type}/${existing.id}`, { data: { type, id: existing.id, attributes } })
    return existing
  }
  return (await client.api("POST", `/v1/${type}`, { data: { type, attributes, relationships } })).data
}

async function uploadScreenshot(client, setId, file) {
  const bytes = readFileSync(file.path)
  const reserved = (await client.api("POST", "/v1/appScreenshots", {
    data: { type: "appScreenshots", attributes: { fileName: file.name, fileSize: bytes.length }, relationships: { appScreenshotSet: ref("appScreenshotSets", setId) } },
  })).data
  for (const operation of reserved.attributes.uploadOperations) await client.put(operation, bytes)
  await client.api("PATCH", `/v1/appScreenshots/${reserved.id}`, {
    data: { type: "appScreenshots", id: reserved.id, attributes: { uploaded: true, sourceFileChecksum: file.md5 } },
  })
  for (let attempt = 0; attempt < 40; attempt += 1) {
    const delivery = (await client.api("GET", `/v1/appScreenshots/${reserved.id}`)).data.attributes.assetDeliveryState
    if (delivery?.state === "COMPLETE") return
    if (delivery?.state === "FAILED") throw new Error(`${file.name}: App Store Connect refused it (${(delivery.errors ?? []).map((e) => e.code).join(", ")})`)
    await sleep(3000)
  }
  throw new Error(`${file.name} was still processing after two minutes`)
}

async function identity(client, context) {
  const { team, bundleId, entitlements } = context
  let bundle = (await client.api("GET", `/v1/bundleIds?filter[identifier]=${bundleId}&filter[platform]=IOS`)).data.find((b) => b.attributes.identifier === bundleId)
  if (!bundle) {
    bundle = (await client.api("POST", "/v1/bundleIds", { data: { type: "bundleIds", attributes: { identifier: bundleId, name: "Rudder", platform: "IOS" } } })).data
    console.log(`registered ${bundleId}`)
  }
  // A bundle id's App ID prefix is its team: this catches another team's key.
  if (bundle.attributes.seedId !== team) {
    throw new Error(`${bundleId} has App ID prefix ${bundle.attributes.seedId}, not ${team} from ios/project.yml: IOS_ASC_KEY_* belongs to another team`)
  }
  const enabled = new Set((await client.api("GET", `/v1/bundleIds/${bundle.id}/bundleIdCapabilities?limit=200`)).data.map((c) => c.attributes.capabilityType))
  const unknown = []
  for (const entitlement of entitlements) {
    if (NO_CAPABILITY.has(entitlement)) continue
    if (GRANTED_BY_APPLE.test(entitlement)) {
      console.log(`${entitlement}: Apple grants it to team ${team} on request; the upload fails until it has`)
      continue
    }
    const capability = ENTITLEMENT_CAPABILITIES[entitlement]
    if (!capability) {
      unknown.push(entitlement)
      continue
    }
    if (enabled.has(capability.capabilityType)) continue
    await client.api("POST", "/v1/bundleIdCapabilities", {
      data: { type: "bundleIdCapabilities", attributes: capability, relationships: { bundleId: ref("bundleIds", bundle.id) } },
    })
    enabled.add(capability.capabilityType)
    console.log(`enabled ${capability.capabilityType} for ${bundleId}`)
  }
  if (unknown.length) throw new Error(`no capability is mapped for ${unknown.join(", ")}: add it to ENTITLEMENT_CAPABILITIES`)
  console.log(`${bundleId} on team ${team}: entitlements ${entitlements.join(", ") || "none"}; capabilities ${[...enabled].join(", ") || "none"}`)
  // The upload needs the app record, which only the website creates.
  const app = await findApp(client, context)
  console.log(`app record ${app.attributes.name} (${app.id})`)
}

async function testflight(client, context) {
  const { build } = context
  const app = await findApp(client, context)
  // Processing usually takes 5 to 15 minutes; the build is not listed at all at first.
  let found
  for (let attempt = 0; attempt < 60; attempt += 1) {
    found = await findBuild(client, app, build)
    const state = found?.attributes.processingState
    console.log(`build ${build}: ${state ?? "not listed yet"}`)
    if (state === "VALID") break
    if (state === "FAILED" || state === "INVALID") throw new Error(`build ${build} processing ${state}`)
    await sleep(30_000)
  }
  if (found?.attributes.processingState !== "VALID") throw new Error(`build ${build} did not finish processing in 30 minutes`)
  console.log(`build ${build} is in TestFlight; internal testers install it from the TestFlight app`)
}

async function prepare(client, context) {
  const { version, build } = context
  const listing = loadListing()
  const sets = checkListing(listing)
  const review = reviewDetails(client.dryRun)
  const app = await findApp(client, context)
  const versions = await iosVersions(client, app)
  const plan = planVersion(versions, version)
  // What's New is refused on an app's first version.
  const released = versions.some((v) => RELEASED.has(v.attributes.appVersionState))
  const versionAttributes = { copyright: listing.copyright, releaseType: listing.releaseType }

  let storeVersion
  if (plan.action === "create") {
    storeVersion = (await client.api("POST", "/v1/appStoreVersions", {
      data: { type: "appStoreVersions", attributes: { platform: "IOS", versionString: version, ...versionAttributes }, relationships: { app: ref("apps", app.id) } },
    })).data
    console.log(`created App Store version ${version}`)
  } else {
    storeVersion = plan.version
    const attributes = plan.action === "rename" ? { versionString: version, ...versionAttributes } : versionAttributes
    await client.api("PATCH", `/v1/appStoreVersions/${storeVersion.id}`, { data: { type: "appStoreVersions", id: storeVersion.id, attributes } })
    console.log(plan.action === "rename"
      ? `renamed App Store version ${plan.version.attributes.versionString} to ${version}`
      : `App Store version ${version} is ${plan.version.attributes.appVersionState}`)
  }

  const localizations = (await client.api("GET", `/v1/appStoreVersions/${storeVersion.id}/appStoreVersionLocalizations?limit=50`)).data
  const versionLocalization = localizations.find((l) => l.attributes.locale === listing.locale)
  const localization = await upsert(client, versionLocalization, "appStoreVersionLocalizations", {
    ...(versionLocalization ? {} : { locale: listing.locale }),
    description: listing.description,
    keywords: listing.keywords,
    marketingUrl: listing.marketingUrl,
    promotionalText: listing.promotionalText,
    supportUrl: listing.supportUrl,
    ...(released ? { whatsNew: listing.whatsNew } : {}),
  }, { appStoreVersion: ref("appStoreVersions", storeVersion.id) })
  console.log(`set the ${listing.locale} description, keywords, promotional text and URLs${released ? " and What's New" : ""}`)

  const appInfo = (await client.api("GET", `/v1/apps/${app.id}/appInfos`)).data.find((info) => EDITABLE_APP_INFO.has(info.attributes.state))
  if (!appInfo) throw new Error("App Store Connect has no editable app info (name, subtitle, categories)")
  await client.api("PATCH", `/v1/appInfos/${appInfo.id}`, {
    data: { type: "appInfos", id: appInfo.id, relationships: { primaryCategory: ref("appCategories", listing.primaryCategory), secondaryCategory: ref("appCategories", listing.secondaryCategory) } },
  })
  const infoLocalizations = (await client.api("GET", `/v1/appInfos/${appInfo.id}/appInfoLocalizations?limit=50`)).data
  const infoLocalization = infoLocalizations.find((l) => l.attributes.locale === listing.locale)
  await upsert(client, infoLocalization, "appInfoLocalizations", {
    ...(infoLocalization ? {} : { locale: listing.locale }),
    name: listing.name,
    subtitle: listing.subtitle,
    privacyPolicyUrl: listing.privacyPolicyUrl,
  }, { appInfo: ref("appInfos", appInfo.id) })
  await client.api("PATCH", `/v1/apps/${app.id}`, { data: { type: "apps", id: app.id, attributes: { contentRightsDeclaration: listing.contentRightsDeclaration } } })
  console.log(`set the name, subtitle, privacy policy, categories and content rights`)

  const full = (await client.api("GET", `/v1/appStoreVersions/${storeVersion.id}?include=appStoreReviewDetail,build`)).data
  const detail = full.relationships.appStoreReviewDetail.data
  const reviewAttributes = { ...review, notes: listing.reviewNotes }
  if (detail) {
    await client.api("PATCH", `/v1/appStoreReviewDetails/${detail.id}`, { data: { type: "appStoreReviewDetails", id: detail.id, attributes: reviewAttributes } }, { quiet: true })
  } else {
    await client.api("POST", "/v1/appStoreReviewDetails", {
      data: { type: "appStoreReviewDetails", attributes: reviewAttributes, relationships: { appStoreVersion: ref("appStoreVersions", storeVersion.id) } },
    }, { quiet: true })
  }
  console.log("set the App Review contact, demo account and notes")

  const uploaded = await findBuild(client, app, build)
  if (!uploaded) throw new Error(`build ${build} is not in App Store Connect: run the upload step first`)
  if (uploaded.attributes.processingState !== "VALID") throw new Error(`build ${build} is ${uploaded.attributes.processingState}; run prepare again once it is VALID`)
  if (full.relationships.build.data?.id !== uploaded.id) {
    await client.api("PATCH", `/v1/appStoreVersions/${storeVersion.id}/relationships/build`, ref("builds", uploaded.id))
  }
  console.log(`build ${build} attached`)

  const existingSets = (await client.api("GET", `/v1/appStoreVersionLocalizations/${localization.id}/appScreenshotSets?limit=50`)).data
  for (const { type, files } of sets) {
    const set = existingSets.find((s) => s.attributes.screenshotDisplayType === type)
      ?? (await client.api("POST", "/v1/appScreenshotSets", {
        data: { type: "appScreenshotSets", attributes: { screenshotDisplayType: type }, relationships: { appStoreVersionLocalization: ref("appStoreVersionLocalizations", localization.id) } },
      })).data
    const current = (await client.api("GET", `/v1/appScreenshotSets/${set.id}/appScreenshots?limit=50`)).data
    const same = current.length === files.length && current.every((shot, i) =>
      shot.attributes.sourceFileChecksum === files[i].md5 && shot.attributes.assetDeliveryState?.state === "COMPLETE")
    if (same) {
      console.log(`${type}: ${files.length} screenshots already there`)
      continue
    }
    for (const shot of current) await client.api("DELETE", `/v1/appScreenshots/${shot.id}`)
    for (const file of files) await uploadScreenshot(client, set.id, file)
    console.log(`${type}: uploaded ${files.length} screenshots`)
  }
  console.log(`${version} is ready for review. Nothing was submitted; submit is its own step.`)
}

async function submit(client, context) {
  const { version, build } = context
  const app = await findApp(client, context)
  const storeVersion = (await iosVersions(client, app)).find((v) => v.attributes.versionString === version)
  if (!storeVersion) throw new Error(`App Store Connect has no version ${version}: run prepare first`)
  if (!EDITABLE.has(storeVersion.attributes.appVersionState)) throw new Error(`${version} is ${storeVersion.attributes.appVersionState}`)
  const full = (await client.api("GET", `/v1/appStoreVersions/${storeVersion.id}?include=build`)).data
  const uploaded = await findBuild(client, app, build)
  if (!uploaded || full.relationships.build.data?.id !== uploaded.id) throw new Error(`${version} does not have build ${build} attached: run prepare`)

  const submissions = (await client.api("GET", `/v1/reviewSubmissions?filter[app]=${app.id}&filter[platform]=IOS&limit=50`)).data
  const busy = submissions.find((s) => ["WAITING_FOR_REVIEW", "IN_REVIEW", "CANCELING", "COMPLETING"].includes(s.attributes.state))
  if (busy) throw new Error(`a review submission is already ${busy.attributes.state}`)
  // An unsubmitted one, or the one a rejection left open: App Store Connect
  // keeps one open submission per platform.
  const submission = submissions.find((s) => ["READY_FOR_REVIEW", "UNRESOLVED_ISSUES"].includes(s.attributes.state))
    ?? (await client.api("POST", "/v1/reviewSubmissions", { data: { type: "reviewSubmissions", attributes: { platform: "IOS" }, relationships: { app: ref("apps", app.id) } } })).data
  const items = (await client.api("GET", `/v1/reviewSubmissions/${submission.id}/items?include=appStoreVersion&limit=50`)).data
  if (!items.some((item) => item.relationships?.appStoreVersion?.data?.id === storeVersion.id)) {
    await client.api("POST", "/v1/reviewSubmissionItems", {
      data: { type: "reviewSubmissionItems", relationships: { reviewSubmission: ref("reviewSubmissions", submission.id), appStoreVersion: ref("appStoreVersions", storeVersion.id) } },
    })
  }
  await client.api("PATCH", `/v1/reviewSubmissions/${submission.id}`, { data: { type: "reviewSubmissions", id: submission.id, attributes: { submitted: true } } })
  console.log(`submitted ${version} (build ${build}) for App Review`)
}

async function release(client, context) {
  const { version } = context
  const app = await findApp(client, context)
  const storeVersion = (await iosVersions(client, app)).find((v) => v.attributes.versionString === version)
  if (storeVersion?.attributes.appVersionState !== "PENDING_DEVELOPER_RELEASE") {
    throw new Error(`${version} is ${storeVersion?.attributes.appVersionState ?? "not in App Store Connect"}, not waiting to be released`)
  }
  await client.api("POST", "/v1/appStoreVersionReleaseRequests", {
    data: { type: "appStoreVersionReleaseRequests", relationships: { appStoreVersion: ref("appStoreVersions", storeVersion.id) } },
  })
  console.log(`released ${version} on the App Store`)
}

async function status(client, context) {
  const app = await findApp(client, context)
  console.log(`app ${app.attributes.name} (${app.id})`)
  for (const v of await iosVersions(client, app)) console.log(`version ${v.attributes.versionString}: ${v.attributes.appVersionState}`)
  const builds = await client.api("GET", `/v1/builds?filter[app]=${app.id}&sort=-uploadedDate&limit=5&include=preReleaseVersion`)
  const trains = Object.fromEntries((builds.included ?? []).map((train) => [train.id, train.attributes.version]))
  for (const b of builds.data) {
    console.log(`build ${b.attributes.version} (${trains[b.relationships?.preReleaseVersion?.data?.id] ?? "?"}): ${b.attributes.processingState}, uploaded ${b.attributes.uploadedDate}`)
  }
  for (const s of (await client.api("GET", `/v1/reviewSubmissions?filter[app]=${app.id}&filter[platform]=IOS&limit=5`)).data) {
    console.log(`review submission ${s.id}: ${s.attributes.state}${s.attributes.submittedDate ? `, submitted ${s.attributes.submittedDate}` : ""}`)
  }
}

const COMMANDS = { check: null, status, identity, testflight, prepare, submit, release }
const CONFIRMED = new Set(["submit", "release"])

async function main(argv) {
  let dryRun = false
  let confirm
  const positional = []
  for (let i = 0; i < argv.length; i += 1) {
    if (argv[i] === "--dry-run") dryRun = true
    else if (argv[i] === "--confirm") confirm = argv[(i += 1)]
    else if (argv[i].startsWith("--")) throw new Error(`unknown option ${argv[i]}`)
    else positional.push(argv[i])
  }
  const [command] = positional
  if (positional.length !== 1 || !(command in COMMANDS)) {
    throw new Error(`usage: ios/scripts/appstore.mjs ${Object.keys(COMMANDS).join("|")} [--confirm X.Y.Z] [--dry-run]`)
  }
  const shipped = { ...shippedVersion(), ...appIdentity() }
  if (CONFIRMED.has(command) && confirm !== shipped.version) {
    throw new Error(`${command} acts on ${shipped.version} for real: pass --confirm ${shipped.version}`)
  }
  if (!CONFIRMED.has(command) && confirm !== undefined) throw new Error(`${command} takes no --confirm`)
  console.log(`${command} ${shipped.bundleId} ${shipped.version} (build ${shipped.build}), team ${shipped.team}${dryRun ? ", dry run: nothing is sent" : ""}`)
  if (command === "check") {
    const sets = checkListing(loadListing())
    console.log(`ios/appstore is ready: ${sets.map((set) => `${set.type} ${set.files.length}`).join(", ")}`)
    return
  }
  const client = dryRun ? dryClient({ command, ...shipped }) : realClient()
  await COMMANDS[command](client, shipped)
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main(process.argv.slice(2)).catch((error) => {
    console.error(error.message)
    process.exit(1)
  })
}
