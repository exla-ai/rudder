#!/usr/bin/env node
// Sets jj (Jujutsu), Rudder's workspace engine, up at install time so
// `npm install -g @exla-ai/rudder` is the only step. When jj is not on PATH it
// fetches jj's own prebuilt release (checksum-pinned in dist/jj-binary.js) into
// ~/.rudder/bin; Rudder puts that on PATH itself. The first `rudder` run does
// the same thing, so anything skipped here (no network, --ignore-scripts, npx)
// costs a few seconds later, never a failed install. Always exits 0.

const TAG = "[rudder postinstall]";

async function main() {
  if (process.env.RUDDER_SKIP_POSTINSTALL) return;
  // `sudo npm install -g` runs this as root, often with the user's HOME: a
  // root-owned ~/.rudder would lock the user's own runs out of it. The first
  // `rudder` run sets jj up as the user instead.
  if (process.getuid?.() === 0) return;
  let jj;
  try {
    jj = await import("../dist/jj-binary.js");
  } catch {
    return; // a source checkout before its first build
  }
  const source = await jj.ensureJjAvailable({ onInstall: (message) => console.log(`${TAG} ${message}`) });
  if (!source) {
    console.warn(`${TAG} jj was not set up now (${jj.managedJjInstallError()}); rudder will retry on its first run.`);
  }
}

try {
  await main();
} catch (err) {
  console.warn(`${TAG} jj setup skipped: ${err && err.message ? err.message : err}`);
}
process.exit(0);
