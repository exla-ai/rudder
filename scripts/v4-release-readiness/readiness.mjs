#!/usr/bin/env node
import { createHash, randomUUID } from 'node:crypto';
import { spawnSync, spawn } from 'node:child_process';
import { createReadStream } from 'node:fs';
import fs from 'node:fs/promises';
import path from 'node:path';
import os from 'node:os';
import { fileURLToPath } from 'node:url';
import { checks, gates, requiredScopes, deliveryRequirements } from './catalog.mjs';

const hash = bytes => createHash('sha256').update(bytes).digest('hex');
const sha = value => typeof value === 'string' && /^[0-9a-f]{64}$/.test(value);
const revision = value => typeof value === 'string' && /^[0-9a-f]{40}$/.test(value);
const must = (condition, reason) => { if (!condition) throw new Error(reason); };
const utc = value => typeof value === 'string' && /Z$/.test(value) && Number.isFinite(Date.parse(value));
const requirementNames = new Set([...gates.flatMap(g => g.requirements), ...deliveryRequirements]);

export async function digestFile(file) {
  const digest = createHash('sha256'); let bytes = 0;
  for await (const chunk of createReadStream(file)) { digest.update(chunk); bytes += chunk.length; }
  return { sha256: digest.digest('hex'), bytes };
}

export async function atomicJson(file, value) {
  await fs.mkdir(path.dirname(file), { recursive: true, mode: 0o700 });
  const temporary = `${file}.${randomUUID()}.tmp`;
  try {
    await fs.writeFile(temporary, JSON.stringify(value, null, 2) + '\n', { mode: 0o600, flag: 'wx' });
    await fs.rename(temporary, file);
  } finally { await fs.rm(temporary, { force: true }); }
}

// Include test/release/config inputs as well as product code. Documentation and
// the generated ownership snapshot do not invalidate product measurements.
export function isCandidateInput(name) {
  return !/^(?:docs\/|examples\/.*\/evidence\/|RUDDER(?:_SHARED)?\.md$|DECISIONS\.md$)/.test(name);
}

export function testCounts(log) {
  const rust = [...log.matchAll(/test result: (?:ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored;/g)];
  if (rust.length) return rust.reduce((sum, match) => ({ passed: sum.passed + Number(match[1]),
    failed: sum.failed + Number(match[2]), skipped: sum.skipped + Number(match[3]) }), { passed: 0, failed: 0, skipped: 0 });
  const tap = key => log.match(new RegExp(`^# ${key} (\\d+)$`, 'm'))?.[1];
  if (tap('pass') !== undefined && tap('fail') !== undefined && tap('skipped') !== undefined)
    return { passed: Number(tap('pass')), failed: Number(tap('fail')), skipped: Number(tap('skipped')) };
  return null;
}

export async function capture(root) {
  const list = spawnSync('jj', ['file', 'list', '--no-pager'], { cwd: root, encoding: 'utf8', maxBuffer: 16 * 1024 * 1024 });
  must(list.status === 0, 'jj file list failed; capture from the candidate workspace');
  const files = list.stdout.trimEnd().split('\n').filter(Boolean).filter(isCandidateInput).sort();
  must(files.length > 0, 'empty candidate');
  const entries = [];
  for (const name of files) {
    must(!path.isAbsolute(name) && !name.split('/').includes('..') && !/[\r\n]/.test(name), 'unsupported candidate path');
    const full = path.join(root, name), stat = await fs.lstat(full);
    if (stat.isSymbolicLink()) entries.push([name, 'symlink', hash(await fs.readlink(full)), 0]);
    else {
      must(stat.isFile(), 'candidate contains a non-file');
      const digest = await digestFile(full);
      entries.push([name, stat.mode & 0o111 ? 'executable' : 'file', digest.sha256, digest.bytes]);
    }
  }
  const commit = spawnSync('jj', ['log', '--no-graph', '--no-pager', '-r', '@', '-T', 'commit_id'], { cwd: root, encoding: 'utf8' });
  must(commit.status === 0 && revision(commit.stdout.trim()), 'candidate revision unavailable');
  return { version: 1, revision: commit.stdout.trim(), tree_sha256: hash(JSON.stringify(entries)), entries };
}

export async function verifyReceipt(receipt, candidate, base) {
  must(receipt?.version === 1 && sha(candidate.tree_sha256) && revision(candidate.revision), 'invalid evidence version or candidate');
  must(receipt.source?.tree_sha256 === candidate.tree_sha256 && receipt.source?.revision === candidate.revision, 'evidence source differs from sealed candidate');
  must(utc(receipt.started_at) && utc(receipt.finished_at) && Date.parse(receipt.finished_at) >= Date.parse(receipt.started_at), 'invalid evidence UTC interval');
  must(receipt.result === 'passed' && receipt.exit_code === 0, 'failed or incomplete observation');
  must(receipt.platform && ['linux', 'darwin'].includes(receipt.platform.os) && ['arm64', 'x64'].includes(receipt.platform.arch)
    && typeof receipt.platform.kernel === 'string' && receipt.platform.kernel.length > 0, 'platform missing');
  must(Array.isArray(receipt.artifacts) && receipt.artifacts.length > 0 && receipt.artifacts.length <= 50, 'evidence artifacts missing');
  for (const artifact of receipt.artifacts) {
    must(typeof artifact.path === 'string' && !path.isAbsolute(artifact.path) && artifact.path.split(/[\\/]/).every(p => p && p !== '..' && p !== '.'), 'artifact escapes evidence directory');
    const file = path.resolve(base, artifact.path), real = await fs.realpath(file);
    const realBase = await fs.realpath(base);
    must(real.startsWith(realBase + path.sep) && (await fs.lstat(file)).isFile(), 'artifact is not a contained regular file');
    const actual = await digestFile(file);
    must(actual.sha256 === artifact.sha256 && actual.bytes === artifact.bytes && actual.bytes > 0, 'artifact digest/size mismatch');
  }
  if (receipt.check) {
    const check = checks[receipt.check];
    must(check && JSON.stringify(receipt.command) === JSON.stringify(check.command) && receipt.scope === check.scope, 'unknown or changed local check');
    must(!check.platform || check.platform === receipt.platform.os, 'check ran on wrong platform');
    must(Array.isArray(receipt.requirements) && receipt.requirements.length === 0, 'local fixture cannot qualify an experiment');
    must(receipt.tests?.passed > 0 && receipt.tests.failed === 0 && receipt.tests.skipped === 0, 'local check executed no tests or skipped/failed cases');
  } else {
    must(Array.isArray(receipt.requirements) && receipt.requirements.length > 0
      && new Set(receipt.requirements).size === receipt.requirements.length, 'qualification requirements missing or duplicated');
    must(receipt.review?.accepted === true && typeof receipt.review.owner === 'string' && receipt.review.owner.trim().length > 0
      && typeof receipt.review.limits === 'string' && receipt.review.limits.trim().length > 0, 'coordinator review and sample limits required');
    must(Number.isSafeInteger(receipt.samples) && receipt.samples > 0 && receipt.skipped === 0 && receipt.failed === 0, 'unrun, skipped or failed qualification');
    for (const name of receipt.requirements) {
      must(requirementNames.has(name), 'unknown qualification requirement');
      if (requiredScopes[name]) must(receipt.scope === requiredScopes[name], `wrong scope for ${name}`);
      if (name.endsWith('-linux')) must(receipt.platform.os === 'linux', 'Linux proof requires Linux');
      if (name.includes('mac-')) {
        must(receipt.platform.os === 'darwin', 'Mac proof requires Darwin');
        must(receipt.platform.translated === false && receipt.platform.hardware_arch === receipt.platform.arch, 'Mac proof requires native hardware without translation');
        if (name.endsWith('arm64')) must(receipt.platform.arch === 'arm64', 'Apple Silicon proof requires arm64');
        if (name.endsWith('x64')) must(receipt.platform.arch === 'x64', 'Intel proof requires x64');
      }
      if (name === 'two-worker-distribution' || name === 'replacement-root-restore') {
        must(Array.isArray(receipt.workers) && receipt.workers.length >= 2, 'independent original and replacement workers required');
        const [a,b] = receipt.workers;
        must(a.machine && b.machine && a.machine !== b.machine && a.volume && b.volume && a.volume !== b.volume
          && b.initially_empty === true, 'shared machine/volume cannot prove remote restore');
      }
    }
  }
  return receipt;
}

export async function evaluate(candidate, records, base) {
  const accepted = new Set(), observations = [], rejected = [];
  for (let index = 0; index < records.length; index++) {
    try {
      const record = await verifyReceipt(records[index], candidate, base);
      observations.push(record.check ?? record.requirements.join(','));
      for (const name of record.requirements) accepted.add(name);
    } catch (error) { rejected.push({ index, reason: error.message }); }
  }
  const results = gates.map(gate => ({ ...gate, status: gate.requirements.every(r => accepted.has(r)) ? 'supported' : 'blocked',
    missing: gate.requirements.filter(r => !accepted.has(r)) }));
  const delivery = deliveryRequirements.filter(r => !accepted.has(r));
  return { version: 1, candidate: { revision: candidate.revision, tree_sha256: candidate.tree_sha256 },
    ready: rejected.length === 0 && results.every(g => g.status === 'supported') && delivery.length === 0,
    gates: results, delivery_missing: delivery, observations, rejected };
}

export async function runCheck(name, root, output, candidate) {
  const check = checks[name]; must(check, 'unknown local check');
  must(!check.platform || check.platform === process.platform, 'local check unavailable on this platform');
  must(JSON.stringify((await capture(root)).entries) === JSON.stringify(candidate.entries), 'candidate changed before check');
  await fs.mkdir(output, { recursive: true, mode: 0o700 });
  const logfile = `${name}-${randomUUID()}.log`, file = await fs.open(path.join(output, logfile), 'wx', 0o600);
  const started = new Date().toISOString(); let exit;
  try {
    exit = await new Promise((resolve, reject) => {
      const child = spawn(check.command[0], check.command.slice(1), { cwd: root, stdio: ['ignore', file.fd, file.fd] });
      child.once('error', reject); child.once('close', code => resolve(code));
    });
  } finally { await file.close(); }
  const after = await capture(root);
  const unchanged = after.tree_sha256 === candidate.tree_sha256 && after.revision === candidate.revision;
  const logStat = await fs.stat(path.join(output, logfile));
  const tests = logStat.size <= 64 * 1024 * 1024 ? testCounts(await fs.readFile(path.join(output, logfile), 'utf8')) : null;
  const receipt = { version: 1, source: { tree_sha256: candidate.tree_sha256, revision: candidate.revision }, check: name,
    command: check.command, scope: check.scope, requirements: [], limits: check.limits,
    platform: { os: process.platform, arch: process.arch, kernel: os.release() },
    started_at: started, finished_at: new Date().toISOString(), exit_code: exit,
    result: exit === 0 && unchanged && tests?.passed > 0 && tests.failed === 0 && tests.skipped === 0 ? 'passed' : 'failed', tests,
    artifacts: [{ path: logfile, ...await digestFile(path.join(output, logfile)) }] };
  await atomicJson(path.join(output, `${name}.json`), receipt);
  return receipt;
}

async function main(args) {
  const root = process.cwd(), [operation, input, output] = args;
  if (operation === 'capture' && input && !output) { await atomicJson(input, await capture(root)); return; }
  if (operation === 'catalog' && !input) { console.log(JSON.stringify({ checks, gates }, null, 2)); return; }
  if (operation === 'check' && input && output) {
    const candidate = JSON.parse(await fs.readFile(path.join(output, 'candidate.json'), 'utf8'));
    const receipt = await runCheck(input, root, output, candidate);
    console.log(JSON.stringify({ check: input, result: receipt.result, limits: receipt.limits }));
    process.exitCode = receipt.result === 'passed' ? 0 : 1; return;
  }
  if (operation === 'report' && input && !output) {
    const candidate = JSON.parse(await fs.readFile(path.join(input, 'candidate.json'), 'utf8'));
    const current = await capture(root);
    must(current.tree_sha256 === candidate.tree_sha256 && current.revision === candidate.revision, 'candidate changed since capture');
    const records = [];
    for (const entry of (await fs.readdir(input)).sort()) {
      if (entry.endsWith('.json') && !['candidate.json', 'report.json'].includes(entry)) records.push(JSON.parse(await fs.readFile(path.join(input, entry), 'utf8')));
    }
    const report = await evaluate(candidate, records, input);
    await atomicJson(path.join(input, 'report.json'), report);
    console.log(JSON.stringify(report, null, 2)); process.exitCode = report.ready ? 0 : 1; return;
  }
  throw new Error('Usage: readiness.mjs catalog | capture <candidate.json> | check <local-check> <evidence-dir> | report <evidence-dir>');
}
if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main(process.argv.slice(2)).catch(() => { console.error('READINESS_REFUSED: invalid input, changed source or unavailable evidence.'); process.exitCode = 2; });
}
