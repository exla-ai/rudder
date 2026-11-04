#!/usr/bin/env node
// Coordinator-run only. Consume already built release bytes; never compile,
// publish, alter a global install or start a scheduler in a user repository.
import fs from 'node:fs/promises';
import path from 'node:path';
import os from 'node:os';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { atomicJson, digestFile } from './readiness.mjs';
import { readNativeBinary } from '../native-artifacts.mjs';

const requireValue = (condition, message) => { if (!condition) throw new Error(message); };

export function validateMacHost({ platform, arch, hardwareArm, translated, major }) {
  requireValue(platform === 'darwin' && ['arm64', 'x64'].includes(arch), 'Use a supported native Mac runner');
  requireValue(major >= 14, 'Mac app requires macOS 14 or newer');
  requireValue(!translated && (hardwareArm ? arch === 'arm64' : arch === 'x64'), 'Use native Node on actual Apple Silicon or Intel hardware, without Rosetta');
}

export function validateMacManifest(manifest, platform, revision) {
  requireValue(manifest?.formatVersion === 1 && manifest.sourceRevision === revision && manifest.platform === platform
    && manifest.buildProfile === 'release' && JSON.stringify(manifest.features) === '["rudder-vcs-bridge/cli"]', 'Installed native artifact source/platform differs from candidate');
  requireValue(manifest.files && Object.keys(manifest.files).sort().join(',') === 'rudder-jj,rudder-native', 'Both native executable manifest entries are required');
  for (const expected of Object.values(manifest.files)) {
    requireValue(expected && /^[0-9a-f]{64}$/.test(expected.sha256)
      && Number.isSafeInteger(expected.size) && expected.size > 0, 'Invalid native executable digest/size');
  }
}

export function validateMacAppProof(proof, revision) {
  requireValue(proof?.version === 1 && proof.source_revision === revision
    && /^https:\/\/github\.com\/exla-ai\/rudder\/actions\/runs\/[0-9]+$/.test(proof.workflow_run)
    && typeof proof.artifact_id === 'string' && proof.artifact_id.trim().length > 0
    && typeof proof.app_version === 'string' && proof.app_version.length > 0
    && typeof proof.app_build === 'string' && proof.app_build.length > 0
    && proof.review?.accepted === true && typeof proof.review.owner === 'string' && proof.review.owner.trim().length > 0,
    'Supply coordinator-reviewed exact-source Mac CI artifact provenance');
  for (const name of ['Contents/MacOS/Rudder', 'Contents/Info.plist']) {
    const expected = proof.files?.[name];
    requireValue(expected && /^[0-9a-f]{64}$/.test(expected.sha256)
      && Number.isSafeInteger(expected.bytes) && expected.bytes > 0, 'Mac app executable/plist digests are required');
  }
}

export async function qualifyMac({ tarball, version, revision, expectedArch, output, app, appProof }) {
  requireValue(/^\d+\.\d+\.\d+(?:-[A-Za-z0-9.-]+)?$/.test(version) && /^[0-9a-f]{40}$/.test(revision), 'Supply exact package version and committed revision');
  requireValue(['arm64', 'x64'].includes(expectedArch) && process.arch === expectedArch, 'Runner architecture does not match requested gate');
  requireValue(process.platform === 'darwin', 'Mac qualification cannot run on Linux');
  requireValue(Boolean(app) === Boolean(appProof), 'Supply both Rudder.app and its reviewed CI provenance file');
  tarball = path.resolve(tarball); output = path.resolve(output);
  requireValue((await fs.lstat(tarball)).isFile() && tarball.endsWith('.tgz'), 'Supply the already qualified npm tarball');
  await fs.mkdir(output, { recursive: true, mode: 0o700 });
  const temporary = await fs.mkdtemp(path.join(os.tmpdir(), 'rudder-mac-qualification-'));
  const started_at = new Date().toISOString(), observations = [];
  // Never inherit a running Rudder owner or a profile selected by this worker.
  const env = { ...process.env };
  for (const key of Object.keys(env)) if (key.startsWith('RUDDER_')) delete env[key];
  Object.assign(env, { HOME: path.join(temporary, 'home'), RUDDER_HOME: path.join(temporary, 'home', '.rudder'),
    CODEX_HOME: path.join(temporary, 'home', '.codex'), CLAUDE_CONFIG_DIR: path.join(temporary, 'home', '.claude'),
    CI: '1', RUDDER_DISABLE_UPDATE_CHECK: '1', RUDDER_DISABLE_AUTO_UPDATE: '1', RUDDER_DISABLE_TELEMETRY: '1' });
  await fs.mkdir(env.HOME, { recursive: true });
  let index = 0;
  const run = async (program, args, includeStderr = false) => {
    const result = spawnSync(program, args, { cwd: temporary, env, encoding: 'utf8', timeout: 300_000, maxBuffer: 8 * 1024 * 1024 });
    const name = `mac-${expectedArch}-${String(++index).padStart(2, '0')}.log`;
    await fs.writeFile(path.join(output, name), (result.stdout ?? '') + (result.stderr ?? ''), { mode: 0o600 });
    observations.push({ program: path.basename(program), args, exit_code: result.status, artifact: { path: name, ...await digestFile(path.join(output, name)) } });
    requireValue(result.status === 0, 'Mac probe failed; retain private log for coordinator review');
    return (includeStderr ? result.stdout + result.stderr : result.stdout).trim();
  };
  let failure = null, binaries, appEvidence;
  try {
    const versionString = await run('/usr/bin/sw_vers', ['-productVersion']);
    const hardwareArm = spawnSync('/usr/sbin/sysctl', ['-n', 'hw.optional.arm64'], { encoding: 'utf8' }).stdout?.trim() === '1';
    const translated = spawnSync('/usr/sbin/sysctl', ['-n', 'sysctl.proc_translated'], { encoding: 'utf8' }).stdout?.trim() === '1';
    validateMacHost({ platform: process.platform, arch: process.arch, hardwareArm, translated, major: Number(versionString.split('.')[0]) });
    await run('npm', ['install', '--prefix', path.join(temporary, 'install'), '--no-audit', '--no-fund', tarball]);
    const installed = path.join(temporary, 'install/node_modules/@exla/rudder');
    const pkg = JSON.parse(await fs.readFile(path.join(installed, 'package.json'), 'utf8'));
    requireValue(pkg.version === version && pkg.name === '@exla/rudder', 'Installed package/version differs from candidate');
    const platform = `darwin-${expectedArch}`, directory = path.join(installed, 'dist/native', platform);
    const manifest = JSON.parse(await fs.readFile(path.join(directory, 'native-manifest.json'), 'utf8'));
    validateMacManifest(manifest, platform, revision);
    binaries = {};
    for (const name of ['rudder-native', 'rudder-jj']) binaries[name] = await readNativeBinary(path.join(directory, name), manifest.files[name]);
    const cli = path.join(installed, 'dist/index.js');
    requireValue(await run(process.execPath, [cli, '--version']) === version, 'Installed CLI reports another version');
    const help = JSON.parse(await run(process.execPath, [cli, 'help', '--json']));
    requireValue(help && Array.isArray(help.operations), 'Installed command registry unavailable');
    const jjVersion = await run(path.join(directory, 'rudder-jj'), ['--version']);
    requireValue(/0\.40\.0/.test(jjVersion), 'Bundled jj does not report pinned version');
    await run(path.join(directory, 'rudder-native'), ['--smoke']);
    await run('git', ['--version']);
    if (app) {
      app = path.resolve(app);
      requireValue(app.endsWith('.app') && (await fs.lstat(app)).isDirectory(), 'Supply a signed, exported Rudder.app');
      const proofDigest = await digestFile(appProof);
      const proof = JSON.parse(await fs.readFile(appProof, 'utf8'));
      validateMacAppProof(proof, revision);
      const files = {};
      for (const name of ['Contents/MacOS/Rudder', 'Contents/Info.plist']) {
        requireValue((await fs.lstat(path.join(app, name))).isFile(), 'Mac app provenance requires regular executable/plist files');
        files[name] = await digestFile(path.join(app, name));
        requireValue(files[name].sha256 === proof.files[name].sha256 && files[name].bytes === proof.files[name].bytes,
          'Mac app bytes differ from reviewed CI artifact');
      }
      const appVersion = await run('/usr/libexec/PlistBuddy', ['-c', 'Print :CFBundleShortVersionString', path.join(app, 'Contents/Info.plist')]);
      const appBuild = await run('/usr/libexec/PlistBuddy', ['-c', 'Print :CFBundleVersion', path.join(app, 'Contents/Info.plist')]);
      requireValue(appVersion === proof.app_version && appBuild === proof.app_build, 'Mac app version/build differs from reviewed artifact');
      await run('/usr/bin/codesign', ['--verify', '--deep', '--strict', app]);
      const signed = await run('/usr/bin/codesign', ['-dv', '--verbose=4', app], true);
      requireValue(/Authority=Developer ID Application/.test(signed)
        && /flags=.*runtime/.test(signed), 'Developer ID and hardened runtime required');
      await run('/usr/bin/xcrun', ['stapler', 'validate', app]);
      await run('/usr/sbin/spctl', ['--assess', '--type', 'execute', '--verbose=2', app]);
      await run('/usr/bin/lipo', ['-verify_arch', 'arm64', 'x86_64', path.join(app, 'Contents/MacOS/Rudder')]);
      for (const [name, before] of Object.entries(files)) {
        requireValue((await digestFile(path.join(app, name))).sha256 === before.sha256, 'Mac app changed during probes');
      }
      requireValue((await digestFile(appProof)).sha256 === proofDigest.sha256, 'Mac app provenance changed during probes');
      appEvidence = { source_revision: revision, workflow_run: proof.workflow_run, artifact_id: proof.artifact_id,
        app_version: appVersion, app_build: appBuild, files, proof: proofDigest,
        team_id: signed.match(/^TeamIdentifier=(.+)$/m)?.[1], code_directory_hash: signed.match(/^CDHash=(.+)$/m)?.[1] };
    }
  } catch (error) { failure = error.message; }
  finally {
    await fs.rm(temporary, { recursive: true, force: true });
    await atomicJson(path.join(output, `mac-${expectedArch}-probe.json`), {
      version: 1, source_revision: revision, package_version: version, tarball: await digestFile(tarball), binaries, app: appEvidence,
      platform: { os: process.platform, arch: process.arch, kernel: os.release() }, started_at,
      finished_at: new Date().toISOString(), result: failure ? 'failed' : 'probes-passed', failure,
      observations, limits: 'Install and executable/signature probes only. Interactive app/TUI launch, actual Codex/Claude turns, Xcode/APFS and laptop gates require separately reviewed observations. Not a qualification receipt.' });
  }
  requireValue(!failure, failure);
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const [tarball, version, revision, expectedArch, output, app, appProof] = process.argv.slice(2);
  if (!tarball || !version || !revision || !expectedArch || !output) {
    console.error('Usage: mac-qualify.mjs <qualified-package.tgz> <version> <revision> <arm64|x64> <private-evidence-dir> [Rudder.app app-proof.json]');
    process.exitCode = 2;
  } else qualifyMac({ tarball, version, revision, expectedArch, output, app, appProof }).catch(() => { console.error('MAC_GATE_PENDING_OR_FAILED; inspect private probe receipt.'); process.exitCode = 1; });
}
