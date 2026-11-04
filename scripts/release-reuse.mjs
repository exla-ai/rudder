#!/usr/bin/env node
// Qualification DATA is accepted only with trusted server run/job/artifact
// evidence. No dispatch input, environment waiver or branch-name shortcut.
import fs from 'node:fs/promises';
import { constants } from 'node:fs';
import { createHash } from 'node:crypto';
import { execFileSync, spawn } from 'node:child_process';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { isDeepStrictEqual } from 'node:util';

export const PLATFORMS = ['darwin-arm64', 'linux-x64'];
const WORKFLOW = '.github/workflows/release.yml';
export const QUALIFY_STEPS = {
  native_archives:'Authenticate original native archives before extraction',
  verify_native:'Verify complete native artifacts and source revision',
  git_config:'Configure git (rebase-first test creates temp repos)', install_jj:'Install jj',
  install:'Install npm dependencies', cloud:'Install + build cloud test fixtures',
  fixtures:'Compile engine and native test fixtures', js:'Typecheck + JS tests',
  rust:'Native tests', jj_provider:'Managed jj provider tests', tui:'TUI end-to-end tests',
};
export const NATIVE_STEPS = ['Board host access preflight', 'Build native pair', 'Stage native artifact',
  'Upload native artifact', 'Record native upload identity', 'Upload native identity'];
const EVIDENCE = 'release-evidence';
const MAX_NATIVE_ZIP = 1100*1024**2;
const NATIVE_FILES = ['native-manifest.json','rudder-jj','rudder-native'];
const hash = bytes => createHash('sha256').update(bytes).digest('hex');
const must = condition => { if (!condition) throw new Error('RELEASE_EVIDENCE_REJECTED'); };
const id = value => (typeof value === 'number' || (typeof value === 'string' && /^[1-9][0-9]*$/.test(value)))
  && Number.isSafeInteger(Number(value)) && Number(value) > 0;
const sha = value => typeof value === 'string' && /^[0-9a-f]{40}$/.test(value);
const digest = value => { must(typeof value === 'string'); const result = value.replace(/^sha256:/, ''); must(/^[0-9a-f]{64}$/.test(result)); return `sha256:${result}`; };
const time = value => { const result = Date.parse(value); must(Number.isFinite(result)); return result; };
const nativeName = platform => `rudder-native-${platform}`;
const copyName = platform => `reuse-${nativeName(platform)}`;

export function context(env, version, event) {
  const result = { repository:env.GITHUB_REPOSITORY, repository_id:Number(env.GITHUB_REPOSITORY_ID),
    run_id:Number(env.GITHUB_RUN_ID), run_attempt:Number(env.GITHUB_RUN_ATTEMPT), sha:env.GITHUB_SHA,
    ref:env.GITHUB_REF, workflow_ref:env.GITHUB_WORKFLOW_REF, version };
  must(/^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/.test(result.repository) && id(result.repository_id)
    && id(result.run_id) && id(result.run_attempt) && sha(result.sha)
    && /^\d+\.\d+\.\d+(?:-[A-Za-z0-9.-]+)?(?:\+[A-Za-z0-9.-]+)?$/.test(version)
    && result.workflow_ref === `${result.repository}/${WORKFLOW}@${result.ref}`
    && env.GITHUB_WORKFLOW_SHA === result.sha && env.GITHUB_EVENT_NAME === 'push'
    && event.ref === result.ref && event.deleted !== true && event.repository?.id === result.repository_id
    && event.repository?.full_name === result.repository && typeof event.repository?.fork === 'boolean');
  return result;
}
const sameContext = (a,b) => isDeepStrictEqual(a,b);
function sameRepository(run, ctx, allowFork=false) {
  return [run.repository,run.head_repository].every(repo => repo?.id === ctx.repository_id
    && repo.full_name === ctx.repository && (allowFork ? typeof repo.fork === 'boolean' : repo.fork === false));
}
function runIdentity(run, ctx, workflowId, allowFork=false) {
  return id(run.id) && run.workflow_id === workflowId && run.head_sha === ctx.sha
    && run.event === 'push' && run.path?.split('@')[0] === WORKFLOW && sameRepository(run,ctx,allowFork)
    && Array.isArray(run.pull_requests) && run.pull_requests.length === 0;
}
async function pages(api, endpoint, field) {
  const all = []; let total;
  for (let page=1; page<=3; page++) {
    const value = await api.json(`${endpoint}${endpoint.includes('?') ? '&' : '?'}per_page=100&page=${page}`);
    must(Number.isSafeInteger(value.total_count) && value.total_count >= 0 && value.total_count <= 300
      && Array.isArray(value[field]) && value[field].length <= 100);
    total ??= value.total_count; must(total === value.total_count); all.push(...value[field]);
    if (all.length === total) return all;
    must(all.length < total && value[field].length === 100);
  }
  throw new Error('RELEASE_EVIDENCE_REJECTED');
}
function successfulSteps(job, names) {
  must(job.status === 'completed' && job.conclusion === 'success' && Array.isArray(job.steps));
  for (const name of names) {
    const matches = job.steps.filter(step => step.name === name);
    must(matches.length === 1 && matches[0].status === 'completed' && matches[0].conclusion === 'success');
  }
}
function checkJobs(jobs, run, cutoff) {
  const required = [...PLATFORMS.map(p=>`Native ${p}`), 'Release qualification'];
  must(new Set(jobs.map(job=>job.name)).size === jobs.length);
  for (const name of required) {
    const job = jobs.find(job=>job.name === name); must(job && id(job.id) && job.run_id === run.id && job.head_sha === run.head_sha);
    must(time(job.completed_at) <= cutoff);
    successfulSteps(job, name === 'Release qualification' ? Object.values(QUALIFY_STEPS) : NATIVE_STEPS);
  }
  for (const job of jobs.filter(job=>!required.includes(job.name))) {
    must(['Release reuse probe','Publish npm release'].includes(job.name)
      && job.status === 'completed' && job.conclusion === 'skipped');
  }
  return required.map(name=>({id:jobs.find(job=>job.name === name).id,name}));
}
function artifact(value, run, ctx, now) {
  must(id(value.id) && value.expired === false && time(value.expires_at) > now
    && Number.isSafeInteger(value.size_in_bytes) && value.size_in_bytes > 0 && value.size_in_bytes <= MAX_NATIVE_ZIP
    && value.workflow_run?.id === run.id && value.workflow_run.repository_id === ctx.repository_id
    && value.workflow_run.head_repository_id === ctx.repository_id && value.workflow_run.head_sha === ctx.sha
    && value.workflow_run.head_branch === run.head_branch);
  return { id:value.id, name:value.name, digest:digest(value.digest), expires_at:value.expires_at, size:value.size_in_bytes };
}
function exactlyOne(values, name) { const matches=values.filter(v=>v.name === name); must(matches.length === 1); return matches[0]; }
function validateManifests(manifests, revision) {
  must(Array.isArray(manifests) && manifests.length === PLATFORMS.length
    && isDeepStrictEqual(manifests.map(m=>m.platform).sort(), [...PLATFORMS].sort()));
  for (const manifest of manifests) {
    must(manifest.formatVersion === 1 && manifest.sourceRevision === revision && manifest.buildProfile === 'release');
    for (const name of ['rudder-native','rudder-jj']) {
      const file=manifest.files?.[name]; must(file && /^[0-9a-f]{64}$/.test(file.sha256)
        && Number.isSafeInteger(file.size) && file.size > 0 && file.size <= 512*1024**2);
    }
  }
}
function checkQualification(record, ctx) {
  must(record?.format_version === 1 && record.mode === 'full' && sameContext(record.context,ctx)
    && /^[0-9a-f]{64}$/.test(record.archive_proof_sha256));
  validateManifests(record.manifests,ctx.sha);
  must(Array.isArray(record.artifacts) && record.artifacts.length === PLATFORMS.length);
  for (const p of PLATFORMS) { const entry=exactlyOne(record.artifacts,nativeName(p)); must(id(entry.id)); digest(entry.digest); }
  must(Object.keys(record.steps).sort().join() === Object.keys(QUALIFY_STEPS).sort().join()
    && Object.values(record.steps).every(value=>value === 'success'));
}

// Strictly hash the SMALL receipt ZIP against trusted REST metadata. v4's
// download action only warns on digest mismatch; that warning is not evidence.
// unzip -p reads stdout only; no archive entry is extracted onto the checkout.
export async function readReceiptZip(bytes, expectedDigest, expectedName='qualification.json') {
  must(['qualification.json','upload.json'].includes(expectedName)
    && Buffer.isBuffer(bytes) && bytes.length > 0 && bytes.length <= 256*1024 && `sha256:${hash(bytes)}` === digest(expectedDigest));
  const temporary=await fs.mkdtemp(path.join(os.tmpdir(),'rudder-release-evidence-'));
  try {
    const file=path.join(temporary,'receipt.zip'); await fs.writeFile(file,bytes,{flag:'wx',mode:0o600});
    const options={encoding:'utf8',maxBuffer:64*1024,timeout:5000,env:{PATH:'/usr/bin:/bin'},stdio:['ignore','pipe','pipe']};
    must(execFileSync('unzip',['-Z1',file],options).trim() === expectedName);
    return JSON.parse(execFileSync('unzip',['-p',file,expectedName],options));
  } finally { await fs.rm(temporary,{recursive:true,force:true}); }
}
export async function selectReuse(ctx, api, now=Date.now) {
  const clock=typeof now === 'function' ? now : ()=>now; // Numeric clocks are only fixture inputs; CLI supplies none.
  if (ctx.ref !== `refs/tags/v${ctx.version}` || ctx.run_attempt !== 1) return null;
  const base=`/repos/${ctx.repository}/actions`;
  const current=await api.json(`${base}/runs/${ctx.run_id}`);
  must(id(current.workflow_id) && runIdentity(current,ctx,current.workflow_id));
  const workflow=await api.json(`${base}/workflows/${current.workflow_id}`);
  must(workflow.path === WORKFLOW && workflow.name === 'Release' && workflow.state === 'active');
  const branch=`release-check/${ctx.version}`;
  const all=await pages(api,`${base}/workflows/${current.workflow_id}/runs?event=push&head_sha=${ctx.sha}`,'workflow_runs');
  const candidates=all.filter(run=>run.id !== ctx.run_id && run.head_branch === branch && runIdentity(run,ctx,current.workflow_id));
  if (candidates.length !== 1) return null; // Failed/running/duplicate candidates are never silently ignored.
  const run=await api.json(`${base}/runs/${candidates[0].id}`);
  must(runIdentity(run,ctx,current.workflow_id) && run.head_branch === branch && run.run_attempt === 1
    && run.status === 'completed' && run.conclusion === 'success'
    && time(run.updated_at) <= time(current.created_at) && time(run.updated_at) <= clock());
  const jobs=checkJobs(await pages(api,`${base}/runs/${run.id}/attempts/1/jobs`,'jobs'),run,time(current.created_at));
  const uploads=await pages(api,`${base}/runs/${run.id}/artifacts`,'artifacts');
  const qualification=artifact(exactlyOne(uploads,'release-qualification'),run,ctx,clock());
  must(qualification.size <= 256*1024);
  const record=await readReceiptZip(await api.archive(qualification.id),qualification.digest);
  const candidateContext={...ctx,run_id:run.id,run_attempt:1,ref:`refs/heads/${branch}`,
    workflow_ref:`${ctx.repository}/${WORKFLOW}@refs/heads/${branch}`};
  checkQualification(record,candidateContext);
  const native=PLATFORMS.map(p=>artifact(exactlyOne(uploads,nativeName(p)),run,ctx,clock()));
  must(uploads.filter(a=>a.name.startsWith('rudder-native-')).length === PLATFORMS.length);
  for (const entry of native) {
    const recorded=exactlyOne(record.artifacts,entry.name);
    must(recorded.id === entry.id && digest(recorded.digest) === entry.digest);
  }
  // Slow metadata/receipt work cannot adopt an artifact after its original expiry.
  const adoptedAt=clock(); must([qualification,...native].every(value=>time(value.expires_at) > adoptedAt));
  return {format_version:1,current:ctx,qualification:record,origin:{run_id:run.id,run_attempt:1,
    workflow_id:run.workflow_id,jobs,qualification_artifact:qualification,artifacts:native}};
}

export function githubClient(token, repository, fetcher=fetch) {
  must(typeof token === 'string' && token.length > 0);
  must(/^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/.test(repository) && !repository.split('/').some(v=>v === '.' || v === '..'));
  async function response(endpoint) {
    must(endpoint.startsWith(`/repos/${repository}/actions/`) && !endpoint.includes('..'));
    return fetcher(`https://api.github.com${endpoint}`,{redirect:'manual',signal:AbortSignal.timeout(10_000),
      headers:{Accept:'application/vnd.github+json',Authorization:`Bearer ${token}`,
        'User-Agent':'rudder-release-qualification','X-GitHub-Api-Version':'2022-11-28'}});
  }
  async function bounded(res, limit) {
    must(res.ok); const chunks=[]; let size=0;
    for await (const chunk of res.body) { size+=chunk.length; must(size <= limit); chunks.push(Buffer.from(chunk)); }
    return Buffer.concat(chunks);
  }
  async function archiveResponse(artifactId, timeout) {
    must(id(artifactId)); const initial=await response(`/repos/${repository}/actions/artifacts/${artifactId}/zip`);
    must(initial.status === 302); const target=new URL(initial.headers.get('location'));
    must(target.protocol === 'https:' && !target.username && !target.password && !target.port
      && (target.hostname.endsWith('.blob.core.windows.net') || target.hostname.endsWith('.githubusercontent.com')
        || target.hostname.endsWith('.amazonaws.com')));
    // Never forward Authorization to the short-lived artifact blob URL.
    const result=await fetcher(target,{redirect:'error',signal:AbortSignal.timeout(timeout)});
    must(result.ok); return result;
  }
  return {
    async json(endpoint) { return JSON.parse((await bounded(await response(endpoint),2*1024**2)).toString('utf8')); },
    async archive(artifactId) { return bounded(await archiveResponse(artifactId,10_000),256*1024); },
    async archiveToFile(entry, destination) {
      must(id(entry.id) && Number.isSafeInteger(entry.size) && entry.size > 0 && entry.size <= MAX_NATIVE_ZIP);
      const expected=digest(entry.digest); const file=await fs.open(destination,'wx',0o600);
      try {
        const actual=await streamToFile((await archiveResponse(entry.id,120_000)).body,file,entry.size);
        must(actual.size === entry.size && actual.digest === expected);
      } catch (error) { await fs.rm(destination,{force:true}); throw error; }
      finally { await file.close(); }
    },
  };
}

async function streamToFile(stream, file, limit) {
  const checksum=createHash('sha256'); let size=0;
  for await (const raw of stream) {
    const chunk=Buffer.from(raw); size+=chunk.length; must(size <= limit); checksum.update(chunk);
    for (let offset=0; offset<chunk.length;) {
      const result=await file.write(chunk,offset,chunk.length-offset); must(result.bytesWritten > 0); offset+=result.bytesWritten;
    }
  }
  return {size,digest:`sha256:${checksum.digest('hex')}`};
}
async function extractNativeFile(archive, name, destination) {
  const file=await fs.open(destination,'wx',0o600);
  try {
    const child=spawn('unzip',['-p',archive,name],{env:{PATH:'/usr/bin:/bin'},stdio:['ignore','pipe','pipe']});
    let failed=false, stderr=0;
    const closed=new Promise(resolve=> {
      child.once('error',()=> { failed=true; }); child.once('close',(code,signal)=>resolve({code,signal}));
    });
    const stop=()=> { failed=true; child.kill('SIGKILL'); };
    const timer=setTimeout(stop,120_000);
    child.stderr.on('data',chunk=> { stderr+=chunk.length; if (stderr > 4096) stop(); });
    try {
      await streamToFile(child.stdout,file,name === 'native-manifest.json' ? 16*1024 : 512*1024**2);
      const result=await closed; must(!failed && result.code === 0 && result.signal === null);
    } catch (error) { stop(); await closed; throw error; }
    finally { clearTimeout(timer); }
  } finally { await file.close(); }
}

// Authenticate the producer's small upload records as well as server metadata.
// No native entry is extracted until its original ZIP hash/size are verified.
export async function authenticateNativeArchives(ctx, identities, api, root, now=Date.now) {
  const base=`/repos/${ctx.repository}/actions`, clock=typeof now === 'function' ? now : ()=>now;
  const run=await api.json(`${base}/runs/${ctx.run_id}`);
  must(run.id === ctx.run_id && run.run_attempt === ctx.run_attempt && id(run.workflow_id)
    && runIdentity(run,ctx,run.workflow_id,true)); // Forks may qualify locally; reuse still excludes them.
  const workflow=await api.json(`${base}/workflows/${run.workflow_id}`);
  must(workflow.path === WORKFLOW && workflow.name === 'Release' && workflow.state === 'active');
  const values=await pages(api,`${base}/runs/${run.id}/artifacts`,'artifacts');
  const native=PLATFORMS.map(p=>artifact(exactlyOne(values,nativeName(p)),run,ctx,clock()));
  must(values.filter(value=>value.name.startsWith('rudder-native-')).length === PLATFORMS.length);
  must(values.filter(value=>value.name.startsWith('native-upload-')).length === PLATFORMS.length);
  const produced=[], sidecars=[];
  for (const p of PLATFORMS) {
    const entry=artifact(exactlyOne(values,`native-upload-${p}`),run,ctx,clock()); sidecars.push(entry); must(entry.size <= 256*1024);
    const bytes=await api.archive(entry.id); must(bytes.length === entry.size);
    const claim=await readReceiptZip(bytes,entry.digest,'upload.json');
    must(sameContext(claim.context,ctx) && claim.name === nativeName(p)); produced.push(claim);
  }
  const original=uploads(native,PLATFORMS.map(nativeName));
  must(isDeepStrictEqual(original,uploads(produced,PLATFORMS.map(nativeName))));
  if (identities !== undefined) must(isDeepStrictEqual(original,uploads(identities,PLATFORMS.map(nativeName))));
  const temporary=await fs.mkdtemp(path.join(os.tmpdir(),'rudder-native-archives-')); let ownedRoot=false;
  try {
    await fs.mkdir(root,{mode:0o700}); ownedRoot=true; // Refuse a preexisting/untrusted extraction tree.
    for (const entry of native) {
      must(time(entry.expires_at) > clock()); const archive=path.join(temporary,'native.zip');
      await api.archiveToFile(entry,archive); // Strict raw ZIP verification precedes even listing entries.
      const names=execFileSync('unzip',['-Z1',archive],{encoding:'utf8',maxBuffer:4096,timeout:5000,
        env:{PATH:'/usr/bin:/bin'},stdio:['ignore','pipe','pipe']}).trim().split('\n');
      must(isDeepStrictEqual(names.sort(),NATIVE_FILES));
      const directory=path.join(root,entry.name); await fs.mkdir(directory,{mode:0o700});
      // Extract stdout into fresh regular files; ZIP symlinks/paths cannot escape.
      for (const name of NATIVE_FILES) await extractNativeFile(archive,name,path.join(directory,name));
      await fs.rm(archive);
    }
    must([...native,...sidecars].every(entry=>time(entry.expires_at) > clock()));
    return original;
  } catch (error) { if (ownedRoot) await fs.rm(root,{recursive:true,force:true}); throw error; }
  finally { await fs.rm(temporary,{recursive:true,force:true}); }
}
async function readJson(file, expectedHash, limit=128*1024) {
  const handle=await fs.open(file,constants.O_RDONLY | constants.O_NOFOLLOW | constants.O_NONBLOCK);
  try {
    const stat=await handle.stat(); must(stat.isFile() && stat.size > 0 && stat.size <= limit);
    const bytes=await handle.readFile(); must(bytes.length === stat.size);
    if (expectedHash !== undefined) must(/^[0-9a-f]{64}$/.test(expectedHash) && hash(bytes) === expectedHash);
    return JSON.parse(bytes.toString('utf8'));
  } finally { await handle.close(); }
}
async function writeJson(file, value) {
  await fs.mkdir(path.dirname(file),{recursive:true}); const bytes=Buffer.from(`${JSON.stringify(value,null,2)}\n`);
  must(bytes.length <= 128*1024); await fs.writeFile(file,bytes,{flag:'wx',mode:0o600}); return hash(bytes);
}
export const output=async (key,value)=> { must(/^[a-z_][a-z0-9_]*$/.test(key) && /^[A-Za-z0-9,._-]*$/.test(String(value)));
  await fs.appendFile(process.env.GITHUB_OUTPUT,`${key}=${value}\n`); };
async function verifiedNative(root,ctx,record) {
  const {verifyNativeArtifacts}=await import('./native-artifacts.mjs');
  const verified=await verifyNativeArtifacts({root,expectedRevision:ctx.sha});
  const manifests=verified.artifacts.map(artifact=>artifact.manifest);
  if (record) checkNativeSeal(manifests,record);
  return manifests;
}
export function checkNativeSeal(manifests, record) {
  validateManifests(manifests,record.context.sha);
  must(isDeepStrictEqual(manifests,record.manifests));
}
function uploads(values,names) {
  must(Array.isArray(values) && values.length === PLATFORMS.length);
  const result=names.map(name=> { const value=exactlyOne(values,name); must(id(value.id));
    return {name,id:Number(value.id),digest:digest(value.digest)}; });
  must(new Set(result.map(value=>value.id)).size === PLATFORMS.length); return result;
}
export function validateReuseProof(record, ctx) {
  must(record?.format_version === 1 && ctx.run_attempt === 1 && sameContext(record.current,ctx)
    && record.origin?.run_attempt === 1 && id(record.origin.run_id) && record.origin.run_id !== ctx.run_id
    && id(record.origin.workflow_id));
  const originContext={...ctx,run_id:record.origin.run_id,run_attempt:1,ref:`refs/heads/release-check/${ctx.version}`,
    workflow_ref:`${ctx.repository}/${WORKFLOW}@refs/heads/release-check/${ctx.version}`};
  checkQualification(record.qualification,originContext);
  const originals=uploads(record.origin.artifacts,PLATFORMS.map(nativeName));
  must(isDeepStrictEqual(originals,uploads(record.qualification.artifacts,PLATFORMS.map(nativeName))));
  uploads(record.copies,PLATFORMS.map(copyName));
  return record.qualification;
}
export async function main(command) {
  const pkg=await readJson('package.json');
  const ctx=context(process.env,pkg.version,await readJson(process.env.GITHUB_EVENT_PATH,undefined,2*1024**2));
  if (command === 'select') {
    await output('found','false');
    try {
      const selected=await selectReuse(ctx,githubClient(process.env.GH_TOKEN,ctx.repository));
      if (!selected) return;
      await writeJson(`${EVIDENCE}/selection.json`,selected);
      await output('run_id',selected.origin.run_id); await output('native_ids',selected.origin.artifacts.map(a=>a.id).join(','));
      await output('found','true');
    } catch { console.log('Release reuse unavailable; full qualification required.'); }
  } else if (command === 'upload-identity') {
    const platform=process.env.NATIVE_PLATFORM; must(PLATFORMS.includes(platform));
    must(id(process.env.NATIVE_ARTIFACT_ID));
    await writeJson(`native-upload-meta/${platform}/upload.json`,{context:ctx,name:nativeName(platform),
      id:Number(process.env.NATIVE_ARTIFACT_ID),digest:digest(process.env.NATIVE_ARTIFACT_DIGEST)});
  } else if (command === 'fetch-native') {
    const artifacts=await authenticateNativeArchives(ctx,undefined,githubClient(process.env.GH_TOKEN,ctx.repository),'prebuilt-native');
    const proof={format_version:1,context:ctx,method:'sha256-before-extraction',artifacts,
      manifests:await verifiedNative('prebuilt-native',ctx)};
    await output('proof_sha256',await writeJson(`${EVIDENCE}/native-archives.json`,proof));
  } else if (command === 'fetch-origin') {
    const selected=await readJson(`${EVIDENCE}/selection.json`);
    const api=githubClient(process.env.GH_TOKEN,ctx.repository);
    must(isDeepStrictEqual(selected,await selectReuse(ctx,api)));
    await authenticateNativeArchives(selected.qualification.context,selected.qualification.artifacts,api,'prebuilt-native');
  } else if (command === 'certify') {
    must(ctx.ref === `refs/heads/release-check/${ctx.version}` || ctx.ref === `refs/tags/v${ctx.version}`);
    const steps=JSON.parse(process.env.QUALIFICATION_STEPS); const outcomes={};
    for (const key of Object.keys(QUALIFY_STEPS)) { must(steps[key]?.outcome === 'success' && steps[key]?.conclusion === 'success'); outcomes[key]='success'; }
    // This trusted step-output hash binds the original archive-authenticated
    // seal, rather than deriving new authority from a warning-only download.
    const proof=await readJson(`${EVIDENCE}/native-archives.json`,process.env.EXPECTED_NATIVE_ARCHIVES_SHA256);
    must(proof.format_version === 1 && proof.method === 'sha256-before-extraction' && sameContext(proof.context,ctx)
      && /^[0-9a-f]{64}$/.test(process.env.EXPECTED_NATIVE_ARCHIVES_SHA256));
    const record={format_version:1,mode:'full',context:ctx,steps:outcomes,
      archive_proof_sha256:process.env.EXPECTED_NATIVE_ARCHIVES_SHA256,
      artifacts:uploads(proof.artifacts,PLATFORMS.map(nativeName)),manifests:await verifiedNative('prebuilt-native',ctx,proof)};
    const proofHash=await writeJson(`${EVIDENCE}/qualification.json`,record);
    await output('proof_sha256',proofHash); await output('native_ids',record.artifacts.map(a=>a.id).join(','));
  } else if (command === 'verify-origin') {
    const selected=await readJson(`${EVIDENCE}/selection.json`);
    must(isDeepStrictEqual(selected,await selectReuse(ctx,githubClient(process.env.GH_TOKEN,ctx.repository))));
    await verifiedNative('prebuilt-native',ctx,selected.qualification);
    // Filesystem hashing is blocking work too: do not adopt evidence that
    // expired/disappeared or became ambiguous while those bytes were checked.
    must(isDeepStrictEqual(selected,await selectReuse(ctx,githubClient(process.env.GH_TOKEN,ctx.repository))));
    await writeJson(`${EVIDENCE}/verified.json`,selected);
  } else if (command === 'record-reuse') {
    const selected=await readJson(`${EVIDENCE}/verified.json`); must(sameContext(selected.current,ctx));
    const copies=uploads(JSON.parse(process.env.REUSE_UPLOADS),PLATFORMS.map(copyName));
    const proof={...selected,copies}; const proofHash=await writeJson(`${EVIDENCE}/reuse.json`,proof);
    await output('proof_sha256',proofHash); await output('native_ids',copies.map(a=>a.id).join(','));
  } else if (command === 'publication') {
    must(ctx.ref === `refs/tags/v${ctx.version}`);
    const reused=process.env.REUSE_QUALIFICATION === 'true';
    const record=await readJson(`${EVIDENCE}/${reused ? 'reuse' : 'qualification'}.json`,process.env.EXPECTED_PROOF_SHA256);
    let qualification;
    if (reused) {
      qualification=validateReuseProof(record,ctx);
      // Known names only; existing native verification rejects extras/symlinks.
      for (const p of PLATFORMS) await fs.rename(`prebuilt-native/${copyName(p)}`,`prebuilt-native/${nativeName(p)}`);
    } else { checkQualification(record,ctx); qualification=record; }
    await verifiedNative('prebuilt-native',ctx,qualification);
    must(id(process.env.PROOF_ARTIFACT_ID));
    await writeJson(`${EVIDENCE}/publication-provenance.json`,{format_version:1,mode:reused?'reused':'fresh',
      publication:ctx,qualification:qualification.context,origin:reused?record.origin:null,
      artifacts:qualification.artifacts,copies:reused?record.copies:null,manifests:qualification.manifests,
      proof_artifact:{id:Number(process.env.PROOF_ARTIFACT_ID),digest:digest(process.env.PROOF_ARTIFACT_DIGEST),
        json_sha256:process.env.EXPECTED_PROOF_SHA256}});
    if (process.env.GITHUB_STEP_SUMMARY) {
      const run=reused?record.origin.run_id:ctx.run_id;
      await fs.appendFile(process.env.GITHUB_STEP_SUMMARY,`Qualification: [run ${run}](https://github.com/${ctx.repository}/actions/runs/${run}) at ${ctx.sha}.\n\n`
        + qualification.artifacts.map(a=>`- ${a.name}: artifact ${a.id}, ${a.digest}\n`).join(''));
    }
  } else throw new Error('RELEASE_EVIDENCE_REJECTED');
}
if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  (process.argv.length === 3 ? main(process.argv[2]) : Promise.reject())
    .catch(()=> { console.error('Release qualification evidence rejected.'); process.exitCode=1; });
}
