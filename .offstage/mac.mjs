// .offstage/mac.mjs: the Mac app's CI on an offstage Mac, driven from any
// runner through the offstage API (.github/workflows/mac.yml runs it on
// Linux). It sends mac/ at HEAD to a desktop sandbox (a headless one cannot
// reach testmanagerd, so xcodebuild test fails there), runs
// mac/scripts/ci.sh one stage per command, and copies the logs and, for a
// release, Rudder.dmg and release.env back to ./offstage-out/mac/.
//
//   OFFSTAGE_API_KEY  the team key
//   RELEASE           "true": also sign, notarize and build the DMG
//   ASC_KEY_ID, ASC_ISSUER_ID, ASC_KEY_P8  the App Store Connect key, release only
//   MAC_DEVELOPER_ID_P12, MAC_DEVELOPER_ID_PASSWORD  the Developer ID certificate, release only
//
// Exits with the failing stage's exit code, or collect's when every stage
// passed but the outputs could not be copied back. The sandbox is killed
// whatever happens.
import { execFileSync } from "node:child_process"
import { mkdirSync, rmSync, writeFileSync } from "node:fs"
import { Sandbox } from "offstage-cloud"

const release = process.env.RELEASE === "true"
const stages = release ? ["check", "sign", "notarize", "dmg"] : ["check"]
const secretNames = ["ASC_KEY_ID", "ASC_ISSUER_ID", "ASC_KEY_P8", "MAC_DEVELOPER_ID_P12", "MAC_DEVELOPER_ID_PASSWORD"]
const secrets = {}
if (release) {
  for (const name of secretNames) {
    if (!process.env[name]) throw new Error(`${name} is not set; a release needs the App Store Connect key and the Developer ID certificate`)
    secrets[name] = process.env[name]
  }
}

const source = execFileSync("git", ["archive", "--format=tar.gz", "HEAD", "mac"], { maxBuffer: 64 * 2 ** 20 })
if (source.length > 9 * 2 ** 20) throw new Error(`mac/ packs to ${source.length} bytes; one write takes at most 10 MiB`)

// A team holds at most 4 sandboxes and desktop slots are few: wait for one.
async function createSandbox() {
  for (let waited = 0; ; waited += 15) {
    try {
      return await Sandbox.create({ session: "gui", name: `rudder-mac-${process.env.GITHUB_RUN_ID ?? "local"}`, timeoutMs: 80 * 60_000, idleTimeoutMs: 5 * 60_000 })
    } catch (error) {
      if (!["concurrency-limit", "no-capacity", "sandbox-not-ready"].includes(error.code) || waited >= 20 * 60) throw error
      console.log(`offstage: ${error.code}, trying again in 15 s`)
      await new Promise((resolve) => setTimeout(resolve, 15_000))
    }
  }
}

// A new sandbox is pending until a desktop slot takes it, and an op gives up
// after about a minute of that. A pending sandbox closes as no-capacity after
// 15 minutes; until then it is free, so wait for it here.
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

const sandbox = await createSandbox()
for (const [signal, exitCode] of [["SIGINT", 130], ["SIGTERM", 143]]) {
  process.once(signal, () => sandbox.kill().finally(() => process.exit(exitCode)))
}

let code = 1
try {
  await waitReady(sandbox)
  console.log(`offstage: sandbox ${sandbox.id}, mac/ ${Math.ceil(source.length / 1024)} KiB`)
  await sandbox.files.write("src.tgz", source)
  const unpack = await sandbox.commands.run("mkdir -p src && tar -xzf src.tgz -C src && rm src.tgz")
  if (unpack.exitCode !== 0) throw new Error(`offstage: unpacking mac/ failed: ${unpack.stdout}${unpack.stderr}`)

  code = 0
  for (const stage of stages) {
    console.log(`::group::${stage}`)
    const envs = { CI: "1", ...(stage === "check" ? {} : secrets) }
    const run = await sandbox.commands.run(`bash mac/scripts/ci.sh ${stage}`, { cwd: "src", envs, timeoutMs: 30 * 60_000 })
    process.stdout.write(run.stdout)
    process.stderr.write(run.stderr)
    console.log("::endgroup::")
    code = run.timedOut ? 124 : (run.exitCode ?? 1)
    console.log(`offstage: ${stage} exit ${code} in ${(run.durationMs / 1000).toFixed(0)} s`)
    if (code !== 0) break
  }

  const pack = await sandbox.commands.run("bash mac/scripts/ci.sh collect", { cwd: "src" })
  if (pack.exitCode === 0) {
    writeFileSync("offstage-out.tgz", await sandbox.files.read("out.tgz", { format: "bytes" }))
    rmSync("offstage-out", { recursive: true, force: true })
    mkdirSync("offstage-out", { recursive: true })
    execFileSync("tar", ["-xzf", "offstage-out.tgz", "-C", "offstage-out"])
    rmSync("offstage-out.tgz")
    console.log("offstage: logs and outputs in ./offstage-out/mac")
  } else {
    // No logs, and for a release no DMG: the run failed even if every stage passed.
    console.log(`offstage: collect exit ${pack.exitCode}: ${pack.stdout.trim()}${pack.stderr.trim()}`)
    if (code === 0) code = pack.exitCode || 1
  }
} finally {
  await sandbox.kill()
  console.log(`offstage: killed ${sandbox.id}`)
}
process.exit(code)
