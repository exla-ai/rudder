import test from 'node:test';
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { PLATFORMS, NATIVE_STEPS, QUALIFY_STEPS, selectReuse, readReceiptZip,
  validateReuseProof, checkNativeSeal, githubClient, authenticateNativeArchives, main, output } from '../scripts/release-reuse.mjs';

const hash = bytes => createHash('sha256').update(bytes).digest('hex');
test('Actions outputs accept the SHA256 proof key and reject output-file injection', async () => {
  const temporary = await fs.mkdtemp(path.join(os.tmpdir(), 'release-output-test-'));
  const file = path.join(temporary, 'outputs'), saved = process.env.GITHUB_OUTPUT;
  process.env.GITHUB_OUTPUT = file;
  try {
    const proof = hash('authenticated original archives');
    await output('proof_sha256', proof);
    await output('native_ids', '300,301,302');
    await output('found', 'true');
    const expected = `proof_sha256=${proof}\nnative_ids=300,301,302\nfound=true\n`;
    assert.equal(await fs.readFile(file, 'utf8'), expected);
    for (const [key, value] of [['proof_sha256\nfound', proof], ['1proof', proof],
      ['proof=sha256', proof], ['proof_sha256', `${proof}\nfound=true`]]) {
      await assert.rejects(output(key, value), { message: 'RELEASE_EVIDENCE_REJECTED' });
    }
    assert.equal(await fs.readFile(file, 'utf8'), expected);
  } finally {
    if (saved === undefined) delete process.env.GITHUB_OUTPUT; else process.env.GITHUB_OUTPUT = saved;
    await fs.rm(temporary, { recursive: true, force: true });
  }
});

function zip(files) {
  const entries=[]; const directory=[]; let offset=0;
  for (const [name,raw] of files) {
    const data=Buffer.from(raw); const filename=Buffer.from(name); let crc=0xffffffff;
    for (const byte of data) { crc^=byte; for (let i=0;i<8;i++) crc=(crc>>>1)^((crc&1)?0xedb88320:0); }
    crc=(crc^0xffffffff)>>>0;
    const local=Buffer.alloc(30); local.writeUInt32LE(0x04034b50); local.writeUInt16LE(20,4);
    local.writeUInt32LE(crc,14); local.writeUInt32LE(data.length,18); local.writeUInt32LE(data.length,22);
    local.writeUInt16LE(filename.length,26);
    const central=Buffer.alloc(46); central.writeUInt32LE(0x02014b50); central.writeUInt16LE(20,4); central.writeUInt16LE(20,6);
    central.writeUInt32LE(crc,16); central.writeUInt32LE(data.length,20); central.writeUInt32LE(data.length,24);
    central.writeUInt16LE(filename.length,28); central.writeUInt32LE(offset,42);
    entries.push(local,filename,data); directory.push(central,filename); offset+=local.length+filename.length+data.length;
  }
  const dir=Buffer.concat(directory); const end=Buffer.alloc(22); end.writeUInt32LE(0x06054b50);
  end.writeUInt16LE(files.length,8); end.writeUInt16LE(files.length,10); end.writeUInt32LE(dir.length,12); end.writeUInt32LE(offset,16);
  return Buffer.concat([...entries,dir,end]);
}
function fixture() {
  const version='4.0.0-alpha.99', repository='test-owner/rudder', sha='a'.repeat(40);
  const ref=`refs/tags/v${version}`, branch=`release-check/${version}`, workflow='.github/workflows/release.yml';
  const ctx={repository,repository_id:7,run_id:200,run_attempt:1,sha,ref,version,
    workflow_ref:`${repository}/${workflow}@${ref}`};
  const repo={id:7,full_name:repository,fork:false};
  const run={id:100,workflow_id:9,path:workflow,head_branch:branch,head_sha:sha,event:'push',
    repository:repo,head_repository:repo,pull_requests:[],status:'completed',conclusion:'success',run_attempt:1,
    created_at:'2026-10-31T12:00:00Z',updated_at:'2026-10-31T12:30:00Z'};
  const current={...run,id:200,head_branch:`v${version}`,status:'in_progress',conclusion:null,
    created_at:'2026-10-31T13:00:00Z',updated_at:'2026-10-31T13:00:00Z'};
  const native=PLATFORMS.map((p,i)=>({name:`rudder-native-${p}`,id:300+i,digest:`sha256:${hash(p)}`}));
  const record={format_version:1,mode:'full',context:{...ctx,run_id:100,ref:`refs/heads/${branch}`,
    workflow_ref:`${repository}/${workflow}@refs/heads/${branch}`},
    archive_proof_sha256:hash('original authenticated archive seal'),
    steps:Object.fromEntries(Object.keys(QUALIFY_STEPS).map(name=>[name,'success'])),artifacts:native,
    manifests:PLATFORMS.map(p=>({formatVersion:1,sourceRevision:sha,platform:p,buildProfile:'release',features:['rudder-vcs-bridge/cli'],
      files:Object.fromEntries(['rudder-native','rudder-jj'].map(name=>[name,{sha256:hash(`${p}/${name}`),size:100}]))}))};
  const wrap=value=>({...value,expired:false,expires_at:'2026-11-30T00:00:00Z',size_in_bytes:1000,
    workflow_run:{id:100,repository_id:7,head_repository_id:7,head_branch:branch,head_sha:sha}});
  const artifacts=native.map(wrap); const receipt=wrap({id:400,name:'release-qualification'}); artifacts.push(receipt);
  const names=[...PLATFORMS.map(p=>`Native ${p}`),'Release qualification'];
  const jobs=names.map((name,i)=>({id:500+i,name,run_id:100,head_sha:sha,status:'completed',conclusion:'success',
    completed_at:'2026-10-31T12:29:00Z',steps:(i===PLATFORMS.length?Object.values(QUALIFY_STEPS):NATIVE_STEPS)
      .map(name=>({name,status:'completed',conclusion:'success'}))}));
  let archive; const runs=[run,current]; let calls=0;
  const result={ctx,run,current,record,artifacts,jobs,runs,
    now:Date.parse('2026-10-31T13:01:00Z'),refresh() { archive=zip([['qualification.json',JSON.stringify(record)]]);
      receipt.digest=`sha256:${hash(archive)}`; receipt.size_in_bytes=archive.length; },
    api:{async json(url) { calls++;
      if (url.endsWith('/runs/200')) return structuredClone(current);
      if (url.endsWith('/runs/100')) return structuredClone(run);
      if (url.endsWith('/workflows/9')) return {name:'Release',path:workflow,state:'active'};
      if (url.includes('/workflows/9/runs?')) return structuredClone({total_count:runs.length,workflow_runs:runs});
      if (url.includes('/attempts/1/jobs?')) return structuredClone({total_count:jobs.length,jobs});
      if (url.includes('/runs/100/artifacts?')) return structuredClone({total_count:artifacts.length,artifacts});
      throw new Error('unexpected fake API request');
    },async archive(id) { assert.equal(id,400); return Buffer.from(archive); }},get calls() { return calls; }};
  result.refresh(); return result;
}
const select=f=>selectReuse(f.ctx,f.api,f.now);

test('only one fully qualified exact-SHA push supplies origin IDs and digests',async()=> {
  const f=fixture(); const selected=await select(f);
  assert.equal(selected.origin.run_id,100); assert.equal(selected.origin.run_attempt,1);
  assert.equal(selected.origin.jobs.length,3);
  assert.deepEqual(selected.origin.artifacts.map(a=>a.id),[300,301]);
  assert.ok(selected.origin.artifacts.every(a=>/^sha256:[a-f0-9]{64}$/.test(a.digest)));
});
test('release-check and repeated current tag attempts never even query reuse evidence',async()=> {
  for (const change of [c=>{c.ref='refs/heads/release-check/4.0.0-alpha.99';},c=>{c.run_attempt=2;}]) {
    const f=fixture(); change(f.ctx); assert.equal(await select(f),null); assert.equal(f.calls,0);
  }
});
test('wrong SHA/workflow/path, forks, PRs, mismatched branch, and current run are excluded',async()=> {
  for (const change of [r=>{r.head_sha='b'.repeat(40);},r=>{r.workflow_id=8;},r=>{r.path='.github/workflows/other.yml';},
    r=>{r.head_repository={...r.head_repository,id:8,fork:true};},r=>{r.event='pull_request';},
    r=>{r.pull_requests=[{number:1}];},r=>{r.head_branch='release-check/other-version';},r=>{r.id=200;}]) {
    const f=fixture(); change(f.run); assert.equal(await select(f),null);
  }
});
test('failed, cancelled, incomplete, later-completed and reattempted candidates cannot be reused',async()=> {
  for (const change of [r=>{r.conclusion='failure';},r=>{r.conclusion='cancelled';},r=>{r.status='in_progress';},
    r=>{r.updated_at='2026-10-31T13:00:01Z';},r=>{r.run_attempt=2;}]) {
    const f=fixture(); change(f.run); await assert.rejects(select(f));
  }
});
test('ambiguous success/success and success/failure sets fall back, not latest-success selection',async()=> {
  for (const conclusion of ['success','failure']) {
    const f=fixture(); f.runs.push({...f.run,id:101,conclusion}); assert.equal(await select(f),null);
  }
});
test('required jobs and steps must run successfully; continuation/skip is no waiver',async()=> {
  for (const change of [f=>{f.jobs.pop();},f=>{f.jobs[0].conclusion='skipped';},
    f=>{f.jobs[2].steps.find(s=>s.name===QUALIFY_STEPS.js).conclusion='skipped';},
    f=>{f.jobs[2].steps.find(s=>s.name===QUALIFY_STEPS.native_archives).conclusion='skipped';},
    f=>{f.jobs[2].steps.find(s=>s.name===QUALIFY_STEPS.rust).conclusion='failure';},
    f=>{f.jobs.push({...f.jobs[0],id:999,name:'Unexpected skipped gate',conclusion:'skipped'});},
    f=>{f.record.steps.js='failure';f.refresh();}]) {
    const f=fixture(); change(f); await assert.rejects(select(f));
  }
});
test('missing, expired, duplicate, digestless or foreign artifact evidence cannot qualify',async()=> {
  for (const change of [f=>{f.artifacts.shift();},f=>{f.artifacts[0].expired=true;},
    f=>{f.artifacts[0].expires_at='2026-10-31T13:00:00Z';},f=>{f.artifacts.push({...f.artifacts[0],id:999});},
    f=>{delete f.artifacts[0].digest;},f=>{f.artifacts[0].workflow_run.head_repository_id=8;},
    f=>{f.record.artifacts[0].id=999;f.refresh();}]) {
    const f=fixture(); change(f); await assert.rejects(select(f));
  }
});
test('a slow receipt read cannot adopt evidence after its original expiry',async()=> {
  const f=fixture(); const archive=f.api.archive;
  f.api.archive=async id=> { const bytes=await archive(id); f.now=Date.parse('2026-12-01T00:00:00Z'); return bytes; };
  await assert.rejects(selectReuse(f.ctx,f.api,()=>f.now));
});
test('receipt context, original upload digests and each native source revision are bound',async()=> {
  for (const change of [r=>{r.context.sha='b'.repeat(40);},r=>{r.context.repository_id=8;},
    r=>{r.context.ref='refs/heads/main';},r=>{r.artifacts[0].digest=`sha256:${'f'.repeat(64)}`;},
    r=>{r.manifests[1].sourceRevision='b'.repeat(40);},r=>{delete r.archive_proof_sha256;}]) {
    const f=fixture(); change(f.record); f.refresh(); await assert.rejects(select(f));
  }
});
test('receipt ZIP needs exact server digest and sole expected entry; no extraction paths',async()=> {
  const bytes=zip([['qualification.json','{"ok":true}']]);
  assert.deepEqual(await readReceiptZip(bytes,hash(bytes)),{ok:true});
  await assert.rejects(readReceiptZip(bytes,'0'.repeat(64)));
  for (const files of [[['../qualification.json','{}']],[['qualification.json','{}'],['extra','{}']],
    [['qualification.json','{}'],['qualification.json','{}']]]) {
    const value=zip(files); await assert.rejects(readReceiptZip(value,hash(value)));
  }
});
test('publication proof retains origin scope, version, upload identities and current copy IDs',async()=> {
  const f=fixture(); const selected=await select(f); const proof={...selected,
    copies:PLATFORMS.map((p,i)=>({name:`reuse-rudder-native-${p}`,id:700+i,digest:`sha256:${hash(p)}`}))};
  assert.equal(validateReuseProof(proof,f.ctx),proof.qualification);
  for (const change of [p=>{p.current.sha='b'.repeat(40);},p=>{p.qualification.context.repository='foreign/repo';},
    p=>{p.qualification.context.ref='refs/heads/main';},p=>{p.origin.run_id=200;},
    p=>{p.origin.artifacts[0].id=999;},p=>{p.copies.pop();}]) {
    const wrong=structuredClone(proof); change(wrong); assert.throws(()=>validateReuseProof(wrong,f.ctx));
  }
});
test('every platform manifest/hash/size must match the bytes qualified at the source revision',()=> {
  const f=fixture(); checkNativeSeal(structuredClone(f.record.manifests),f.record);
  for (let p=0;p<PLATFORMS.length;p++) for (const key of ['sha256','size']) {
    const actual=structuredClone(f.record.manifests);
    actual[p].files['rudder-native'][key]=key==='size'?101:'f'.repeat(64);
    assert.throws(()=>checkNativeSeal(actual,f.record));
  }
});
test('artifact redirect never receives the GitHub token and unknown destinations reject',async()=> {
  const requests=[]; const f=fixture(); const bytes=zip([['qualification.json','{}']]);
  const fetcher=async(url,options)=> { requests.push([String(url),options]);
    return requests.length===1 ? new Response(null,{status:302,headers:{location:'https://fixture.blob.core.windows.net/artifact?private=signed'}}) : new Response(bytes); };
  const api=githubClient('fake-read-token',f.ctx.repository,fetcher);
  assert.deepEqual(await api.archive(400),bytes);
  assert.equal(requests[0][1].headers.Authorization,'Bearer fake-read-token');
  assert.equal(requests[1][1].headers,undefined);
  await assert.rejects(githubClient('fake',f.ctx.repository,async()=>new Response(null,{status:302,headers:{location:'http://127.0.0.1/private'}})).archive(400));
});

function nativeArchiveFixture() {
  const f=fixture(), ctx=f.record.context, blobs=new Map(), requests=[];
  const manifests=PLATFORMS.map(p=>({formatVersion:1,sourceRevision:ctx.sha,platform:p,buildProfile:'release',
    features:['rudder-vcs-bridge/cli'],files:{'rudder-native':{sha256:hash('producer-native'),size:15},
      'rudder-jj':{sha256:hash('producer-jj'),size:11}}}));
  const metadata=f.artifacts.slice(0,PLATFORMS.length).map((entry,i)=> {
    const bytes=zip([['native-manifest.json',JSON.stringify(manifests[i])],['rudder-native','producer-native'],['rudder-jj','producer-jj']]);
    blobs.set(entry.id,bytes); return {...entry,digest:`sha256:${hash(bytes)}`,size_in_bytes:bytes.length};
  });
  const identities=metadata.map(entry=>({context:ctx,name:entry.name,id:entry.id,digest:entry.digest}));
  const refreshProducer=()=> {
    const n=PLATFORMS.length;
    for (let i=0;i<n;i++) {
      const bytes=zip([['upload.json',JSON.stringify(identities[i])]]); blobs.set(600+i,bytes);
      metadata[n+i]={...metadata[i],id:600+i,name:`native-upload-${PLATFORMS[i]}`,digest:`sha256:${hash(bytes)}`,size_in_bytes:bytes.length};
    }
  };
  refreshProducer();
  const fetcher=async(raw,options)=> {
    const url=new URL(raw); requests.push({url,options});
    if (url.hostname === 'fixture.blob.core.windows.net') return new Response(blobs.get(Number(url.pathname.slice(1))));
    assert.equal(url.hostname,'api.github.com'); const base=`/repos/${ctx.repository}/actions`;
    if (url.pathname === `${base}/runs/100`) return Response.json(f.run);
    if (url.pathname === `${base}/workflows/9`) return Response.json({name:'Release',path:'.github/workflows/release.yml',state:'active'});
    if (url.pathname === `${base}/runs/100/artifacts`) return Response.json({total_count:metadata.length,artifacts:metadata});
    const match=url.pathname.match(/\/artifacts\/([0-9]+)\/zip$/); assert.ok(match,'unexpected fixture endpoint');
    return new Response(null,{status:302,headers:{location:`https://fixture.blob.core.windows.net/${match[1]}`}});
  };
  return {ctx,metadata,identities,manifests,blobs,requests,fetcher,refreshProducer,now:f.now,api:githubClient('fake-read-token',ctx.repository,fetcher)};
}
const absent=async file=>assert.rejects(fs.stat(file),{code:'ENOENT'});

test('native ZIPs match producer IDs/digests and trusted metadata before exact regular-file extraction',{timeout:10_000},async()=> {
  const f=nativeArchiveFixture(), temporary=await fs.mkdtemp(path.join(os.tmpdir(),'release-native-test-'));
  try {
    const root=path.join(temporary,'native');
    const artifacts=await authenticateNativeArchives(f.ctx,f.identities,f.api,root,f.now);
    assert.deepEqual(artifacts,f.identities.map(({context,...entry})=>entry));
    for (const p of PLATFORMS) {
      const file=path.join(root,`rudder-native-${p}`,'rudder-native');
      assert.equal(await fs.readFile(file,'utf8'),'producer-native');
      assert.equal((await fs.lstat(file)).isSymbolicLink(),false);
    }
    const blobs=f.requests.filter(request=>request.url.hostname === 'fixture.blob.core.windows.net');
    assert.equal(blobs.length,2*PLATFORMS.length); assert.ok(blobs.every(request=>request.options.headers === undefined));
  } finally { await fs.rm(temporary,{recursive:true,force:true}); }
});

test('substituting Linux binary AND matching manifest cannot create an archive seal or qualification receipt',{timeout:10_000},async()=> {
  const f=nativeArchiveFixture(), temporary=await fs.mkdtemp(path.join(os.tmpdir(),'release-native-test-'));
  const original=f.blobs.get(301), changed=structuredClone(f.manifests[1]);
  for (const entry of f.metadata) entry.expires_at=new Date(Date.now()+60_000).toISOString();
  changed.files['rudder-native'].sha256=hash('attacker-native');
  const substituted=zip([['native-manifest.json',JSON.stringify(changed)],['rudder-native','attacker-native'],['rudder-jj','producer-jj']]);
  assert.equal(substituted.length,original.length); assert.notEqual(hash(substituted),hash(original)); f.blobs.set(301,substituted);
  const event=path.join(temporary,'event.json'), output=path.join(temporary,'output');
  const env={GITHUB_REPOSITORY:f.ctx.repository,GITHUB_REPOSITORY_ID:String(f.ctx.repository_id),
    GITHUB_RUN_ID:String(f.ctx.run_id),GITHUB_RUN_ATTEMPT:'1',GITHUB_SHA:f.ctx.sha,GITHUB_REF:f.ctx.ref,
    GITHUB_WORKFLOW_REF:f.ctx.workflow_ref,GITHUB_WORKFLOW_SHA:f.ctx.sha,GITHUB_EVENT_NAME:'push',
    GITHUB_EVENT_PATH:event,GITHUB_OUTPUT:output,GH_TOKEN:'fake-read-token',
    QUALIFICATION_STEPS:JSON.stringify(Object.fromEntries(Object.keys(QUALIFY_STEPS).map(key=>[key,{outcome:'success',conclusion:'success'}])))};
  const cwd=process.cwd(), fetcher=globalThis.fetch, saved=Object.fromEntries(Object.keys(env).map(key=>[key,process.env[key]]));
  const expected=process.env.EXPECTED_NATIVE_ARCHIVES_SHA256;
  try {
    await fs.writeFile(path.join(temporary,'package.json'),JSON.stringify({version:f.ctx.version}));
    await fs.writeFile(event,JSON.stringify({ref:f.ctx.ref,repository:{id:f.ctx.repository_id,full_name:f.ctx.repository,fork:false}}));
    Object.assign(process.env,env); delete process.env.EXPECTED_NATIVE_ARCHIVES_SHA256; process.chdir(temporary); globalThis.fetch=f.fetcher;
    // Assert the digest guard's error, so a later verifier/import failure
    // cannot make this regression pass after archive authentication breaks.
    await assert.rejects(main('fetch-native'),{message:'RELEASE_EVIDENCE_REJECTED'});
    assert.deepEqual(f.requests.filter(request=>request.url.hostname === 'fixture.blob.core.windows.net' && Number(request.url.pathname.slice(1)) < 600)
      .map(request=>Number(request.url.pathname.slice(1))),[300,301]);
    await absent('prebuilt-native'); await absent('release-evidence/native-archives.json'); await absent(output);
    await assert.rejects(main('certify')); await absent('release-evidence/qualification.json');
  } finally {
    process.chdir(cwd); globalThis.fetch=fetcher;
    for (const [key,value] of Object.entries(saved)) { if (value === undefined) delete process.env[key]; else process.env[key]=value; }
    if (expected === undefined) delete process.env.EXPECTED_NATIVE_ARCHIVES_SHA256; else process.env.EXPECTED_NATIVE_ARCHIVES_SHA256=expected;
    await fs.rm(temporary,{recursive:true,force:true});
  }
});

test('forged producer identity or foreign metadata is rejected before any native blob/extraction',{timeout:10_000},async()=> {
  for (const change of [f=>{f.identities[0].id=999;},f=>{f.identities[0].digest=`sha256:${hash('forged')}`;},
    f=>{f.blobs.set(600,zip([['upload.json',JSON.stringify({...f.identities[0],id:999})]]));},
    f=>{f.metadata[0].digest=`sha256:${hash('forged')}`;},
    f=>{f.metadata[0].workflow_run={...f.metadata[0].workflow_run,head_sha:'b'.repeat(40)};}]) {
    const f=nativeArchiveFixture(), temporary=await fs.mkdtemp(path.join(os.tmpdir(),'release-native-test-'));
    try {
      change(f); const root=path.join(temporary,'native');
      await assert.rejects(authenticateNativeArchives(f.ctx,f.identities,f.api,root,f.now)); await absent(root);
      assert.equal(f.requests.filter(request=>request.url.hostname === 'fixture.blob.core.windows.net' && Number(request.url.pathname.slice(1)) < 600).length,0);
    } finally { await fs.rm(temporary,{recursive:true,force:true}); }
  }
});

test('authenticated ZIPs still reject unexpected paths and bounded-entry expansion',{timeout:10_000},async()=> {
  for (const files of [[['../rudder-native','producer-native'],['rudder-jj','producer-jj'],['native-manifest.json','{}']],
    [['rudder-native','producer-native'],['rudder-jj','producer-jj'],['native-manifest.json','x'.repeat(16*1024+1)]]]) {
    const f=nativeArchiveFixture(), temporary=await fs.mkdtemp(path.join(os.tmpdir(),'release-native-test-'));
    const bytes=zip(files); f.blobs.set(301,bytes); f.metadata[1].size_in_bytes=bytes.length;
    f.metadata[1].digest=f.identities[1].digest=`sha256:${hash(bytes)}`;
    f.refreshProducer();
    try {
      const root=path.join(temporary,'native'); await assert.rejects(authenticateNativeArchives(f.ctx,f.identities,f.api,root,f.now)); await absent(root);
      assert.deepEqual(f.requests.filter(request=>request.url.hostname === 'fixture.blob.core.windows.net' && Number(request.url.pathname.slice(1)) < 600)
        .map(request=>Number(request.url.pathname.slice(1))),[300,301]);
    } finally { await fs.rm(temporary,{recursive:true,force:true}); }
  }
});

test('workflow preserves platform/test gates and publishes only from two explicit complete paths',async()=> {
  const yaml=await fs.readFile(new URL('../.github/workflows/release.yml',import.meta.url),'utf8');
  for (const pair of ['key: darwin-arm64\n            runner: macos-15','key: linux-x64\n            runner: ubuntu-22.04']) assert.ok(yaml.includes(pair));
  assert.match(yaml,/github\.ref_type != 'tag' \|\| needs\.reuse\.result != 'success'/);
  assert.match(yaml,/needs\.native-artifacts\.result == 'skipped' && needs\.qualification\.result == 'skipped'/);
  assert.match(yaml,/needs\.native-artifacts\.result == 'success' && needs\.qualification\.result == 'success'/);
  for (const [id,name] of Object.entries(QUALIFY_STEPS)) assert.ok(yaml.includes(`- name: ${name}\n        id: ${id}`));
  assert.ok(yaml.indexOf('run: node scripts/release-reuse.mjs fetch-native') < yaml.indexOf('id: verify_native'));
  assert.ok(yaml.includes('EXPECTED_NATIVE_ARCHIVES_SHA256: ${{ steps.native_archives.outputs.proof_sha256 }}'));
  assert.ok(!yaml.includes('pattern: rudder-native-*')); // No warning-only original native extraction.
  assert.equal((yaml.match(/npm publish --provenance --access public/g)||[]).length,1);
  assert.ok(yaml.includes('npm deprecate @viraatdas/rudder "Retired. Rudder is @exla/rudder:'));
  assert.ok(!yaml.includes('npm pkg set name=@viraatdas/rudder'));
  assert.ok(!yaml.includes('workflow_dispatch:') && !yaml.includes('workflow_run:'));
});
