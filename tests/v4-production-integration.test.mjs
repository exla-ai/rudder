import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import { existsSync, readFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { registerHooks, stripTypeScriptTypes } from 'node:module';
import { EventEmitter } from 'node:events';
import { createServer } from 'node:http';
import { spawnSync } from 'node:child_process';
import { Readable, Writable } from 'node:stream';
import { v4ScratchPrefix } from './helpers/v4-scratch.mjs';
const roots = ['../src/', '../cloud/src/'].map(p => fileURLToPath(new URL(p, import.meta.url)));
registerHooks({resolve(specifier, context, next) {
  if (specifier.startsWith('.') && specifier.endsWith('.js') && context.parentURL) {
    const url = new URL(specifier.slice(0, -3) + '.ts', context.parentURL);
    if (url.protocol === 'file:' && roots.some(root => fileURLToPath(url).startsWith(root)) && existsSync(url)) return next(url.href, context);
  }
  return next(specifier, context);
}, load(url, context, next) {
  // Production is compiled by tsc. The source harness must also transform
  // parameter properties in its transitive imports, rather than strip only.
  if (url.startsWith('file:') && url.endsWith('.ts') && roots.some(root => fileURLToPath(url).startsWith(root))) {
    return {format:'module',shortCircuit:true,source:stripTypeScriptTypes(readFileSync(new URL(url),'utf8'),{mode:'transform',sourceUrl:url})};
  }
  return next(url, context);
}});
const { HomeProjectFiles } = await import('../src/project-files/home.ts');
const { BoardProjectFiles } = await import('../src/board/project-files.ts');
const { ProjectFileRoutes } = await import('../cloud/src/project-files/routes.ts');
const { HomeControlSessions } = await import('../cloud/src/home-control-sessions.ts');
const { assertLegacyWriterAllowed } = await import('../src/engine-authority.ts');
const { BackendTelemetryObserver } = await import('../src/agent-efficiency/backend-observer.ts');
const { classifyComposerInput } = await import('../src/operations/registry.ts');
const { runMigrationCaptureCommand } = await import('../src/v4-migration/cli.ts');
const { verifyRecoveryBundle } = await import('../src/v4-migration/recovery.ts');
const { runProjectFilesCommand } = await import('../src/project-files/cli.ts');
const { applyCommandSurface } = await import('../src/operations/cli.ts');
const { findRepoRoot } = await import('../src/git.ts');
const scope = {projectId:'project', workspaceId:'execution-workspace', attemptId:'attempt', ownerEpoch:2};
const request = {version:1, operation:'read', scope, path:'README.md'};
test('CLI root resolution stays inside its actual jj workspace',()=>{
  const root=fileURLToPath(new URL('../',import.meta.url));
  assert.equal(findRepoRoot(root),path.resolve(root));
});
test('production composer preserves absolute paths and backend slash ownership',()=>{
  for (const text of ['/workspace/project/file.ts fix this','/Users/me/project/file.md']) {
    assert.equal(classifyComposerInput(text,'project').kind,'text');
  }
  assert.equal(classifyComposerInput('/compact','project').kind,'refused');
  assert.equal(classifyComposerInput('/compact','backend').kind,'backend');
  assert.equal(classifyComposerInput('/gam','project').kind,'refused');
});
test('project-files developer command resolves current workspace identity before opening its private board', async()=>{
  const parsed=applyCommandSurface({command:'project-files',args:['execution-workspace','docs/'],flags:{}});
  assert.equal(parsed.command,'project-files');assert.deepEqual(parsed.args,['execution-workspace','docs/']);
  const opened=[];const dependencies={grants:async(repo,signal)=>{assert.equal(repo,'/repo');signal.throwIfAborted();return [{scope,root:'/repo/task'}];},open:async(...args)=>{opened.push(args);}};
  await runProjectFilesCommand('/repo',parsed.args,dependencies);
  assert.deepEqual(opened,[['/repo',{scope,path:'docs',kind:'folder'}]]);
  await assert.rejects(runProjectFilesCommand('/repo',['old-attempt'],dependencies),/Choose a current workspace/);
  await assert.rejects(runProjectFilesCommand('/repo',['execution-workspace','../outside'],dependencies));
  await assert.rejects(runProjectFilesCommand('/repo',[],{...dependencies,grants:async()=>[]}),/No current task workspace/);
  assert.equal(opened.length,1);
});
function query(prefix, overrides={}) {return prefix + '?' + new URLSearchParams({version:'1', operation:'read', ...scope, path:'README.md', ...overrides});}
async function temp(t) {
  const root = await fs.mkdtemp(v4ScratchPrefix('production-test'));
  t.after(() => fs.rm(root, {recursive:true,force:true})); return root;
}
async function serve(t, handler) {
  const server = createServer((req,res) => { void handler(req,res,new URL(req.url,'http://localhost')).catch(e=>{res.statusCode=500;res.end(String(e));}); });
  await new Promise(resolve => server.listen(0,'127.0.0.1',resolve));
  t.after(() => new Promise(resolve => { server.closeAllConnections();server.close(resolve); }));
  return `http://127.0.0.1:${server.address().port}`;
}
test('Cloud ACL routing retains execution scope and home rechecks custody after filesystem read', async t => {
  const root = await temp(t); await fs.writeFile(path.join(root,'README.md'),'current content');
  let custody = true, acl = true, afterRead = false, calls = 0;
  const home = new HomeProjectFiles(async (repo,signal) => {
    assert.equal(repo,root);signal.throwIfAborted();calls++;
    if (afterRead && calls > 1) custody = false;
    return custody ? [{scope,root}] : [];
  });
  const routes = new ProjectFileRoutes({workspace(account, workspace) {
    assert.equal(account,'account');assert.equal(workspace,'cloud-repository');
    if (!acl) throw new Error('revoked');return {homeId:'home'};
  },async rpc(homeId,method,params,signal) {
    assert.equal(homeId,'home');assert.equal(params.workspaceId,'cloud-repository');
    if (method === 'projectFileScopes') return home.scopes(root,signal);
    assert.deepEqual(params.request.scope,scope);return home.dispatch(root,params.request,signal);
  }});
  const base = await serve(t,(req,res,url) => routes.handle(req,res,url,'account'));
  let response = await fetch(base+query('/api/app/project-files'));assert.equal(response.status,403);
  response = await fetch(base+'/api/app/project-file-scopes?workspaceId=cloud-repository');
  assert.deepEqual(await response.json(),{scopes:[scope]});
  response = await fetch(base+query('/api/app/project-files'));assert.equal(response.status,200);
  assert.equal((await response.json()).text,'current content');
  calls=0;afterRead=true;
  response = await fetch(base+query('/api/app/project-files'));assert.equal(response.status,403);
  custody=true;afterRead=false;acl=false;
  response=await fetch(base+query('/api/app/project-files'));assert.equal(response.status,403);
});
test('board file routes bind the authenticated session and refuse old owner epochs', async t => {
  const root=await temp(t);await fs.writeFile(path.join(root,'README.md'),'board file');
  let valid=true;
  const session={};const board = new BoardProjectFiles(async()=>[{scope,root}]);
  const base=await serve(t,(req,res,url)=>board.handle(req,res,url,root,{valid:()=>valid},session));
  let response=await fetch(base+'/file-scopes');assert.deepEqual(await response.json(),{scopes:[scope]});
  response=await fetch(base+query('/files'));assert.equal(response.status,200);
  response=await fetch(base+query('/files',{ownerEpoch:'1'}));assert.equal(response.status,403);
  valid=false;response=await fetch(base+query('/files'));assert.equal(response.status,403);
});
test('Cloud file routing keeps simultaneous account bindings separate', async t => {
  const roots = {alice:await temp(t), bob:await temp(t)};
  for (const [actor,root] of Object.entries(roots)) await fs.writeFile(path.join(root,'README.md'),actor);
  const homes = Object.fromEntries(Object.keys(roots).map(actor => [actor,
    new HomeProjectFiles(async()=>[{scope,root:roots[actor]}])]));
  const routes = new ProjectFileRoutes({
    workspace(actor, repository) {
      assert.ok(Object.hasOwn(roots,actor));assert.equal(repository,'repository');return {homeId:actor};
    },
    async rpc(home,method,params,signal) {
      assert.equal(params.workspaceId,'repository');
      return method === 'projectFileScopes' ? homes[home].scopes(roots[home],signal)
        : homes[home].dispatch(roots[home],params.request,signal);
    },
  });
  const base = await serve(t,(req,res,url)=>routes.handle(req,res,url,String(req.headers['x-test-actor'])));
  for (const actor of Object.keys(roots)) {
    const response=await fetch(base+'/api/app/project-file-scopes?workspaceId=repository',{headers:{'x-test-actor':actor}});
    assert.equal(response.status,200);
  }
  for (const actor of Object.keys(roots)) {
    const response=await fetch(base+query('/api/app/project-files'),{headers:{'x-test-actor':actor}});
    assert.equal(response.status,200);assert.equal((await response.json()).text,actor);
  }
});
test('shared legacy admission rejects every migration marker including broken links', async t => {
  const root=await temp(t);assert.doesNotThrow(()=>assertLegacyWriterAllowed(root));
  const engine=path.join(root,'.rudder/engine');await fs.mkdir(engine,{recursive:true});
  const marker=path.join(engine,'v4-migration.json');
  for (const text of ['{','{"phase":"committed"}','{"phase":"rolled_back"}']) {
    await fs.writeFile(marker,text);assert.throws(()=>assertLegacyWriterAllowed(root),/explicit recovery/);
  }
  await fs.unlink(marker);await fs.symlink('absent',marker);
  assert.throws(()=>assertLegacyWriterAllowed(root),/explicit recovery/);
});
test('migration helper captures a real ordinary jj checkout without a scheduler', async t=>{
  const root=await temp(t), repo=path.join(root,'repo'), bundle=path.join(root,'recovery');
  const init=spawnSync('jj',['git','init','--colocate',repo],{encoding:'utf8'});
  assert.equal(init.status,0,init.stderr);
  await fs.writeFile(path.join(repo,'README.md'),'uncommitted source to preserve');
  let output='';
  await runMigrationCaptureCommand(Readable.from([JSON.stringify({repository:repo,bundle,
    projectId:'project',migrationId:'migration',sessions:[]})]),
    new Writable({write(chunk,encoding,done){output+=chunk.toString();done();}}));
  assert.deepEqual(JSON.parse(output),{format_version:1,captured:true});
  const manifest=await verifyRecoveryBundle(bundle);
  assert.equal(manifest.project_id,'project');assert.equal(manifest.migration_id,'migration');
  assert.equal(manifest.jj_version,'0.40.0');
  assert.ok(manifest.entries.some(entry=>entry.path==='README.md'));
  assert.equal(existsSync(path.join(repo,'.rudder/engine/owner.json')),false);
});
test('file RPC cancellation retires pending delivery and never replays a late reply', async()=>{
  const socket={};const sent=[];const sessions=new HomeControlSessions({isOpen:()=>true,send:(_,data)=>{sent.push(JSON.parse(data));return true;}});
  const owner=sessions.attach('home',socket);const controller=new AbortController();
  const pending=sessions.request('home',socket,'projectFiles',{},1000,controller.signal);
  const rejected=assert.rejects(pending,{reason:'cancelled'});controller.abort();await rejected;
  assert.equal(owner.pending.size,0);assert.equal(sessions.reply(owner,sent[0].id,{}),false);assert.equal(sent.length,1);
  await assert.rejects(sessions.request('home',socket,'projectFiles',{},1000,controller.signal),{reason:'cancelled'});
  assert.equal(sent.length,1);
});
test('file RPC errors without results retire pending delivery', async()=>{
  const socket={};const sent=[];const sessions=new HomeControlSessions({isOpen:()=>true,send:(_,data)=>{sent.push(JSON.parse(data));return true;}});
  const owner=sessions.attach('home',socket);
  const pending=sessions.request('home',socket,'projectFiles',{},1000);
  const rejected=assert.rejects(pending,{message:'Workspace is unavailable',status:400});
  assert.equal(sessions.reply(owner,sent[0].id,undefined,'Workspace is unavailable'),true);
  await rejected;assert.equal(owner.pending.size,0);
  assert.equal(sessions.reply(owner,sent[0].id,{}),false);
});
test('provider stream observer deduplicates repeated per-step usage and persists no transcript', async t=>{
  const root=await temp(t);const observer=new BackendTelemetryObserver({repoRoot:root,id:'run',backend:'codex',model:'gpt-6.1-sol'});
  const event={type:'event_msg',payload:{type:'token_count',info:{total_token_usage:{input_tokens:100},last_token_usage:{input_tokens:10,output_tokens:2}}},secret:'private transcript'};
  await observer.observe(event);await observer.observe(event);
  const text=await fs.readFile(path.join(root,'.rudder/runs/run/efficiency.json'),'utf8');
  const data=JSON.parse(text);assert.equal(data.steps.length,1);assert.equal(data.steps[0].tokens.input,10);assert.doesNotMatch(text,/private transcript|total_token_usage/);
});
