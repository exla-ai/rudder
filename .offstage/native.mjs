// Run the macOS release build on Offstage from the exact GitHub revision.
import { execFileSync } from "node:child_process"
import { createHash } from "node:crypto"
import { appendFileSync, chmodSync, copyFileSync, lstatSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs"
import { tmpdir } from "node:os"
import { join } from "node:path"
import { setTimeout as sleep } from "node:timers/promises"
import { Task } from "offstage-cloud"

const repo = "https://github.com/exla-ai/rudder.git"
const revision = process.env.GITHUB_SHA
const head = execFileSync("git", ["rev-parse", "HEAD"], { encoding: "utf8" }).trim()
if (!/^[a-f0-9]{40}$/.test(revision ?? "") || revision !== head) {
  throw new Error(`Offstage native build expects GITHUB_SHA to match the checked-out commit (${head})`)
}

const archiveName = "native-release.tgz"
const checksumName = "native-release.sha256"
const logPath = "offstage-out/native/build.log"
mkdirSync("offstage-out/native", { recursive: true })
writeFileSync(logPath, "")

function log(line) {
  console.log(line)
  appendFileSync(logPath, `${line}\n`)
}

const timeoutMs = 20 * 60_000
const queueTimeoutMs = 15 * 60_000
const task = await Task.create({
  repo,
  kind: "build",
  session: "headless",
  timeoutMs,
  queueTimeoutMs,
  command: `set -e
git fetch --quiet --depth 1 origin ${revision}
git checkout --quiet --detach FETCH_HEAD
test "$(git rev-parse HEAD)" = ${revision}
echo "Building exact source revision ${revision}"
bash .offstage/native-build.sh`,
  artifacts: [archiveName, checksumName],
})
log(`Offstage task ${task.id}, darwin-arm64, revision ${revision}`)

async function cancelTask() {
  try {
    await task.cancel()
  } catch (error) {
    if (error.code !== "already-ended") throw error
  }
}

for (const [signal, code] of [["SIGINT", 130], ["SIGTERM", 143]]) {
  process.once(signal, () => {
    cancelTask().catch(error => console.error(`Could not cancel Offstage task ${task.id}: ${error.message}`))
      .finally(() => process.exit(code))
  })
}

let nextLogLine = 0
let previousState
function readProgress(info) {
  if (info.state !== previousState) {
    log(`Offstage task ${task.id}: ${info.state}`)
    previousState = info.state
  }
  const dropped = info.droppedLogLines
  // Once capped, the first log event is a display marker, not a log line.
  const logs = info.events.filter(event => event.type === "log").slice(dropped > 0 ? 1 : 0)
  if (nextLogLine < dropped) {
    log(`Offstage trimmed ${dropped - nextLogLine} log lines before they could be fetched`)
    nextLogLine = dropped
  }
  for (const event of logs.slice(nextLogLine - dropped)) log(event.data ?? "")
  nextLogLine = dropped + logs.length
}

try {
  const deadline = Date.now() + timeoutMs + queueTimeoutMs + 5 * 60_000
  readProgress(task.info)
  while (!task.done && Date.now() < deadline) {
    await sleep(10_000)
    readProgress(await task.refresh())
  }
  if (!task.done) throw new Error(`Offstage task ${task.id} exceeded the CI wait limit`)
  if (task.state !== "succeeded") {
    throw new Error(`Offstage task ${task.id}: ${task.state} (${task.info.failureReason}, exit ${task.info.exitCode})`)
  }
} catch (error) {
  if (!task.done) {
    try {
      await cancelTask()
    } catch (cancelError) {
      console.error(`Could not cancel Offstage task ${task.id}: ${cancelError.message}`)
    }
  }
  throw error
}

const checksum = Buffer.from(await task.artifact(checksumName)).toString("utf8").trim()
const bytes = await task.artifact(archiveName)
const hash = createHash("sha256").update(bytes).digest("hex")
if (checksum !== `${hash}  ${archiveName}`) {
  throw new Error(`Offstage task ${task.id} artifact failed SHA-256 verification`)
}

const temp = mkdtempSync(join(tmpdir(), "rudder-offstage-native-"))
const files = ["rudder-native", "rudder-jj", "native-manifest.json"]
const destination = "native-dist/darwin-arm64"
try {
  const archive = join(temp, archiveName)
  writeFileSync(archive, bytes)
  const members = execFileSync("tar", ["-tzf", archive], { encoding: "utf8", maxBuffer: 4096 }).trim().split("\n")
  if (members.length !== files.length || !files.every(name => members.includes(name))) {
    throw new Error(`Offstage task ${task.id} returned an unexpected archive layout`)
  }
  execFileSync("tar", ["-xzf", archive, "-C", temp, ...files])
  for (const name of files) {
    const file = lstatSync(join(temp, name))
    if (!file.isFile() || file.size === 0) throw new Error(`Offstage produced an invalid ${name} artifact`)
  }
  mkdirSync(destination, { recursive: true })
  for (const name of files) {
    const file = join(destination, name)
    copyFileSync(join(temp, name), file)
    if (name !== "native-manifest.json") chmodSync(file, 0o755)
  }
  log(`Downloaded and verified ${bytes.length} bytes from Offstage task ${task.id}`)
} finally {
  rmSync(temp, { recursive: true, force: true })
}
