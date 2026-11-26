// .offstage/ios-appstore.mjs: archives the iPhone app on an Offstage Mac and
// uploads it to App Store Connect, driven from any runner through the
// Offstage API (.github/workflows/ios-appstore.yml runs it on Linux). It sends
// ios/ at HEAD (without the store listing) to a sandbox, runs
// ios/scripts/appstore.sh one stage per command and copies the logs and
// upload.env back to ./offstage-out/ios-appstore/.
//
//   OFFSTAGE_API_KEY                       the team key
//   ASC_KEY_ID, ASC_ISSUER_ID, ASC_KEY_P8  the App Store Connect key (Admin), for upload only
//
// Exits with the failing stage's exit code, or collect's when every stage
// passed but the logs could not be copied back. The sandbox is killed
// whatever happens.
import { execFileSync } from "node:child_process"
import { mkdirSync, rmSync, writeFileSync } from "node:fs"
import { Sandbox } from "offstage-cloud"

const stages = ["archive", "upload"]
const stageSecrets = { archive: [], upload: ["ASC_KEY_ID", "ASC_ISSUER_ID", "ASC_KEY_P8"] }
const stageEnvs = Object.fromEntries(stages.map(stage => {
  const envs = { CI: "1" }
  for (const name of stageSecrets[stage]) {
    if (!process.env[name]) throw new Error(`${name} is required for ${stage}`)
    envs[name] = process.env[name]
  }
  return [stage, envs]
}))

const source = execFileSync("git", ["archive", "--format=tar.gz", "HEAD", "ios", ":(exclude)ios/appstore"], { maxBuffer: 64 * 2 ** 20 })
if (source.length > 9 * 2 ** 20) throw new Error(`ios/ packs to ${source.length} bytes; one write takes at most 10 MiB`)

// A team holds at most 4 sandboxes and desktop slots are few: wait for one.
async function createSandbox() {
  for (let waited = 0; ; waited += 15) {
    try {
      return await Sandbox.create({ session: "gui", name: `rudder-ios-appstore-${process.env.GITHUB_RUN_ID ?? "local"}`, timeoutMs: 60 * 60_000, idleTimeoutMs: 5 * 60_000 })
    } catch (error) {
      if (!["concurrency-limit", "no-capacity", "sandbox-not-ready"].includes(error.code) || waited >= 20 * 60) throw error
      console.log(`offstage: ${error.code}, trying again in 15 s`)
      await new Promise((resolve) => setTimeout(resolve, 15_000))
    }
  }
}

// A new sandbox is pending until a desktop slot takes it.
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
  console.log(`offstage: sandbox ${sandbox.id}, ios/ ${Math.ceil(source.length / 1024)} KiB`)
  await sandbox.files.write("src.tgz", source)
  const unpack = await sandbox.commands.run("mkdir -p src && tar -xzf src.tgz -C src && rm src.tgz")
  if (unpack.exitCode !== 0) throw new Error(`offstage: unpacking ios/ failed: ${unpack.stdout}${unpack.stderr}`)

  code = 0
  for (const stage of stages) {
    console.log(`::group::${stage}`)
    const run = await sandbox.commands.run(`bash ios/scripts/appstore.sh ${stage}`, { cwd: "src", envs: stageEnvs[stage], timeoutMs: 30 * 60_000 })
    process.stdout.write(run.stdout)
    process.stderr.write(run.stderr)
    console.log("::endgroup::")
    code = run.timedOut ? 124 : (run.exitCode ?? 1)
    console.log(`offstage: ${stage} exit ${code} in ${(run.durationMs / 1000).toFixed(0)} s`)
    if (code !== 0) break
  }

  const pack = await sandbox.commands.run("bash ios/scripts/appstore.sh collect", { cwd: "src" })
  if (pack.exitCode === 0) {
    writeFileSync("offstage-out.tgz", await sandbox.files.read("out.tgz", { format: "bytes" }))
    rmSync("offstage-out/ios-appstore", { recursive: true, force: true })
    mkdirSync("offstage-out/ios-appstore", { recursive: true })
    execFileSync("tar", ["-xzf", "offstage-out.tgz", "-C", "offstage-out/ios-appstore", "--strip-components=1"])
    rmSync("offstage-out.tgz")
    console.log("offstage: logs and upload.env in ./offstage-out/ios-appstore")
  } else {
    console.log(`offstage: collect exit ${pack.exitCode}: ${pack.stdout.trim()}${pack.stderr.trim()}`)
    if (code === 0) code = pack.exitCode || 1
  }
} finally {
  await sandbox.kill()
  console.log(`offstage: killed ${sandbox.id}`)
}
process.exit(code)
