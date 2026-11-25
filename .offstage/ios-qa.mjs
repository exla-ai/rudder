// .offstage/ios-qa.mjs: the iPhone app's click-through on an Offstage Mac. It
// sends ios/ at HEAD to a desktop sandbox, builds the app and its UI test
// for the simulator, signs the app in with a pair code minted here, runs
// ios/UITests/steps.json and copies the video, screenshots and logs back to
// ./offstage-out/ios-qa/.
//
//   OFFSTAGE_API_KEY  the team key
//   RUDDER_QA_TOKEN   an rdr_ token for the account to test as; only the
//                     ten-minute pair code made from it reaches the Mac
//   RUDDER_QA_SERVER  the control plane (default https://rudder-cloud-control.fly.dev)
//
// Exits with the failing stage's exit code. The sandbox is killed whatever
// happens.
import { execFileSync } from "node:child_process"
import { mkdirSync, rmSync, writeFileSync } from "node:fs"
import { Sandbox } from "offstage-cloud"

const token = process.env.RUDDER_QA_TOKEN?.trim()
if (!token) throw new Error("RUDDER_QA_TOKEN is required")
const server = process.env.RUDDER_QA_SERVER ?? "https://rudder-cloud-control.fly.dev"

const source = execFileSync("git", ["archive", "--format=tar.gz", "HEAD", "ios"], { maxBuffer: 64 * 2 ** 20 })
if (source.length > 9 * 2 ** 20) throw new Error(`ios/ packs to ${source.length} bytes; one write takes at most 10 MiB`)

async function createSandbox() {
  for (let waited = 0; ; waited += 15) {
    try {
      return await Sandbox.create({ session: "gui", name: `rudder-ios-qa-${process.env.GITHUB_RUN_ID ?? Date.now()}`, timeoutMs: 60 * 60_000, idleTimeoutMs: 5 * 60_000 })
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

  for (const stage of ["build", "run"]) {
    const envs = stage === "run" ? { QA_PAIR_CODE: await pairCode(), QA_SERVER: server } : {}
    const run = await sandbox.commands.run(`bash ios/scripts/qa.sh ${stage}`, { cwd: "src", envs, timeoutMs: 30 * 60_000 })
    process.stdout.write(run.stdout)
    process.stderr.write(run.stderr)
    code = run.timedOut ? 124 : (run.exitCode ?? 1)
    console.log(`offstage: ${stage} exit ${code} in ${(run.durationMs / 1000).toFixed(0)} s`)
    // A missed step still leaves a video worth reading; a failed build leaves nothing to run.
    if (code !== 0 && stage === "build") break
  }

  const pack = await sandbox.commands.run("bash ios/scripts/qa.sh collect", { cwd: "src" })
  if (pack.exitCode !== 0) throw new Error(`offstage: collect exit ${pack.exitCode}: ${pack.stdout.trim()}${pack.stderr.trim()}`)
  writeFileSync("offstage-out.tgz", await sandbox.files.read("out.tgz", { format: "bytes" }))
  rmSync("offstage-out/ios-qa", { recursive: true, force: true })
  mkdirSync("offstage-out/ios-qa", { recursive: true })
  execFileSync("tar", ["-xzf", "offstage-out.tgz", "-C", "offstage-out/ios-qa", "--strip-components=1"])
  rmSync("offstage-out.tgz")
  console.log("offstage: video, screenshots and logs in ./offstage-out/ios-qa")
} finally {
  await sandbox.kill()
  console.log(`offstage: killed ${sandbox.id}`)
}
process.exit(code)
