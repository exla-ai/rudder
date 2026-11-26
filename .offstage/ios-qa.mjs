// .offstage/ios-qa.mjs: the iPhone app on an Offstage Mac's simulators. It
// sends ios/ at HEAD to a desktop sandbox, builds the app and its UI test
// for the simulator, signs the app in with a pair code minted here and runs
// one of two passes, copying the output back to ./offstage-out/ios-qa/:
//
//   QA_PASS=clickthrough  ios/UITests/steps.json on an iPhone, as the account
//                         behind RUDDER_QA_TOKEN: video, screenshots, logs
//   QA_PASS=appstore      ios/UITests/appstore.json on each App Store
//                         screenshot device below, as the demo account behind
//                         RUDDER_DEMO_TOKEN: appstore/<display type>/*.jpg
//
//   OFFSTAGE_API_KEY  the team key
//   RUDDER_QA_TOKEN, RUDDER_DEMO_TOKEN  rdr_ tokens; only the ten-minute pair
//                     codes made from them reach the Mac
//   RUDDER_QA_SERVER  the control plane (default https://rudder-cloud-control.fly.dev)
//
// Exits with the failing stage's exit code. The sandbox is killed whatever
// happens.
import { execFileSync } from "node:child_process"
import { mkdirSync, rmSync, writeFileSync } from "node:fs"
import { Sandbox } from "offstage-cloud"

// App Store screenshot sets (ios/appstore/screenshots/<type>) and the
// simulator that makes each at the size the set takes.
const STORE_DEVICES = [
  ["APP_IPHONE_67", "iPhone 18 Pro Max"],
  ["APP_IPHONE_61", "iPhone 18 Pro"],
  ["APP_IPAD_PRO_3GEN_129", "iPad Pro 13-inch (M5)"],
]

const pass = process.env.QA_PASS
if (!["clickthrough", "appstore"].includes(pass)) throw new Error("QA_PASS must be clickthrough or appstore")
// The App Store screenshots are public: they always come from the demo account.
const tokenName = pass === "appstore" ? "RUDDER_DEMO_TOKEN" : "RUDDER_QA_TOKEN"
const token = process.env[tokenName]?.trim()
if (!token) throw new Error(`${tokenName} is required for the ${pass} pass`)
const server = process.env.RUDDER_QA_SERVER ?? "https://rudder-cloud-control.fly.dev"

const source = execFileSync("git", ["archive", "--format=tar.gz", "HEAD", "ios", ":(exclude)ios/appstore"], { maxBuffer: 64 * 2 ** 20 })
if (source.length > 9 * 2 ** 20) throw new Error(`ios/ packs to ${source.length} bytes; one write takes at most 10 MiB`)

async function createSandbox() {
  for (let waited = 0; ; waited += 15) {
    try {
      return await Sandbox.create({ session: "gui", name: `rudder-ios-qa-${process.env.GITHUB_RUN_ID ?? Date.now()}`, timeoutMs: 80 * 60_000, idleTimeoutMs: 5 * 60_000 })
    } catch (error) {
      if (!["concurrency-limit", "no-capacity", "sandbox-not-ready"].includes(error.code) || waited >= 20 * 60) throw error
      console.log(`offstage: ${error.code}, trying again in 15 s`)
      await new Promise((resolve) => setTimeout(resolve, 15_000))
    }
  }
}

async function waitReady(sandbox) {
  const started = Date.now()
  for (;;) {
    const info = await sandbox.getInfo()
    if (info.state === "ready") return
    if (info.state === "closed") throw new Error(`offstage: sandbox ${sandbox.id} closed before a desktop slot took it (${info.closeReason})`)
    console.log(`offstage: waiting for a desktop slot (${Math.round((Date.now() - started) / 1000)} s)`)
    await new Promise((resolve) => setTimeout(resolve, 15_000))
  }
}

async function pairCode() {
  const response = await fetch(`${server}/api/app/pair/start`, { method: "POST", headers: { authorization: `Bearer ${token}` } })
  if (!response.ok) throw new Error(`pair/start answered ${response.status}`)
  const { code } = await response.json()
  if (!code) throw new Error("pair/start gave no code")
  return code
}

const sandbox = await createSandbox()
for (const [signal, exitCode] of [["SIGINT", 130], ["SIGTERM", 143]]) {
  process.once(signal, () => sandbox.kill().finally(() => process.exit(exitCode)))
}

let code = 1
try {
  await waitReady(sandbox)
  console.log(`offstage: sandbox ${sandbox.id}, ios/ ${Math.ceil(source.length / 1024)} KiB`)
  await sandbox.files.write("src.tgz", source)
  const unpack = await sandbox.commands.run("mkdir -p src && tar -xzf src.tgz -C src && rm src.tgz")
  if (unpack.exitCode !== 0) throw new Error(`offstage: unpacking ios/ failed: ${unpack.stdout}${unpack.stderr}`)

  // Each run stage gets its own pair code, minted just before it starts.
  const runs = pass === "appstore"
    ? STORE_DEVICES.map(([type, device]) => ["shots", { QA_DEVICE: device, QA_DISPLAY_TYPE: type }])
    : [["run", {}]]
  code = 0
  for (const [stage, extra] of [["build", null], ...runs]) {
    const envs = extra ? { ...extra, QA_PAIR_CODE: await pairCode(), QA_SERVER: server } : {}
    const run = await sandbox.commands.run(`bash ios/scripts/qa.sh ${stage}`, { cwd: "src", envs, timeoutMs: 30 * 60_000 })
    process.stdout.write(run.stdout)
    process.stderr.write(run.stderr)
    const exit = run.timedOut ? 124 : (run.exitCode ?? 1)
    console.log(`offstage: ${stage} exit ${exit} in ${(run.durationMs / 1000).toFixed(0)} s`)
    if (code === 0) code = exit
    // A missed step still leaves output worth reading; a failed build leaves nothing to run.
    if (exit !== 0 && stage === "build") break
  }

  const pack = await sandbox.commands.run("bash ios/scripts/qa.sh collect", { cwd: "src" })
  if (pack.exitCode !== 0) throw new Error(`offstage: collect exit ${pack.exitCode}: ${pack.stdout.trim()}${pack.stderr.trim()}`)
  const parts = []
  for (const name of pack.stdout.trim().split("\n")) parts.push(await sandbox.files.read(name, { format: "bytes" }))
  writeFileSync("offstage-out.tgz", Buffer.concat(parts))
  rmSync("offstage-out/ios-qa", { recursive: true, force: true })
  mkdirSync("offstage-out/ios-qa", { recursive: true })
  execFileSync("tar", ["-xzf", "offstage-out.tgz", "-C", "offstage-out/ios-qa", "--strip-components=1"])
  rmSync("offstage-out.tgz")
  console.log("offstage: output in ./offstage-out/ios-qa")
} finally {
  await sandbox.kill()
  console.log(`offstage: killed ${sandbox.id}`)
}
process.exit(code)
