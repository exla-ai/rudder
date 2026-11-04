import assert from 'node:assert/strict';
import test from 'node:test';
import fs from 'node:fs/promises';
import { existsSync } from 'node:fs';
import path from 'node:path';
import { registerHooks } from 'node:module';
import { fileURLToPath } from 'node:url';
import { spawnSync } from 'node:child_process';
import { Readable, Writable } from 'node:stream';
import { randomUUID } from 'node:crypto';
import { v4ScratchPrefix } from './helpers/v4-scratch.mjs';
const source = fileURLToPath(new URL('../src/',import.meta.url));
registerHooks({resolve(specifier, context, next) {
  if(specifier.startsWith('.') && specifier.endsWith('.js') && context.parentURL) {
    const url=new URL(specifier.slice(0,-3)+'.ts',context.parentURL);
    if(url.protocol==='file:' && fileURLToPath(url).startsWith(source) && existsSync(url))return next(url.href,context);
  }
  return next(specifier,context);
}});
const {runMigrationCaptureCommand}=await import('../src/v4-migration/cli.ts');
const {verifyRecoveryBundle}=await import('../src/v4-migration/recovery.ts');
const {assertLegacyWriterAllowed}=await import('../src/engine-authority.ts');
const {assertNoPendingV4Migration}=await import('../src/v4-migration/capture.ts');

async function helper(request) {
  let output='';
  await runMigrationCaptureCommand(Readable.from([JSON.stringify(request)]),new Writable({write(chunk,_,done){output+=chunk;done();}}));
  return JSON.parse(output);
}
async function fixture(t) {
  const root=await fs.mkdtemp(v4ScratchPrefix('migration-production'));
  t.after(()=>fs.rm(root,{recursive:true,force:true}));
  const repo=path.join(root,'repo');
  const initialized=spawnSync('jj',['git','init','--colocate',repo],{encoding:'utf8'});
  assert.equal(initialized.status,0,initialized.stderr);
  const workspace=path.join(root,'workspace');await fs.mkdir(workspace);
  await fs.writeFile(path.join(repo,'source.md'),'original integration source');
  await fs.writeFile(path.join(workspace,'pending.md'),'original unmerged source');
  const transcript=path.join(root,'session.jsonl');await fs.writeFile(transcript,'original session');
  const record={id:'retained',task:'recover my work',status:'running',backend:'codex',
    model:'account-model',effort:'high',worktree:{enabled:true,path:workspace,jjChangeId:'retained-change'},
    session:{nativeSessionId:'retained-session'},process:{pid:1234},merge:{operationId:'accepted-op'}};
  const run=path.join(repo,'.rudder/runs/retained');await fs.mkdir(run,{recursive:true});
  await fs.writeFile(path.join(run,'run.json'),JSON.stringify(record));
  const identity={repository:repo,projectId:'project',migrationId:'migration',sessions:[{runId:'retained',files:[transcript]}]};
  return {root,repo,workspace,transcript,run,record,identity};
}

test('production migration helper captures and exports a checkpoint into an explicitly resumable paused checkout',async t=>{
  const f=await fixture(t), original=path.join(f.root,'original');
  assert.deepEqual(await helper({...f.identity,bundle:original}),{format_version:1,captured:true});
  await fs.writeFile(path.join(f.repo,'source.md'),'post-upgrade integration delta');
  await fs.writeFile(path.join(f.workspace,'pending.md'),'post-upgrade workspace delta');
  await fs.writeFile(f.transcript,'post-upgrade session delta');
  const checkpoint=randomUUID(), delta=path.join(f.root,'delta');
  await helper({operation:'capture',...f.identity,bundle:delta,rollbackCheckpoint:checkpoint});
  const manifest=await verifyRecoveryBundle(delta);assert.equal(manifest.rollback_checkpoint,checkpoint);
  const destination=path.join(f.root,'recovered');
  const result=await helper({operation:'export',bundle:delta,destination,projectId:'project',migrationId:'migration',rollbackCheckpoint:checkpoint});
  assert.equal(result.exported,true);
  const recovery=JSON.parse(await fs.readFile(path.join(result.integrationPath,'.rudder/engine/v4-recovery.json'),'utf8'));
  assert.equal(recovery.ready,true); assert.equal(recovery.repository,await fs.realpath(result.integrationPath));
  assert.equal(recovery.rollback_checkpoint,checkpoint); assert.deepEqual(recovery.run_ids,['retained']);
  assert.throws(()=>assertLegacyWriterAllowed(result.integrationPath),/explicit recovery/);
  await assert.rejects(assertNoPendingV4Migration(result.integrationPath),/explicit recovery/);
  assert.equal(existsSync(path.join(result.integrationPath,'.rudder/engine/owner.json')),false);
  const restored=JSON.parse(await fs.readFile(path.join(result.integrationPath,'.rudder/runs/retained/run.json'),'utf8'));
  assert.equal(restored.status,'paused');assert.equal(restored.process,undefined);assert.equal(restored.terminal,undefined);
  assert.equal(restored.session.nativeSessionId,'retained-session');assert.equal(restored.model,'account-model');
  assert.equal(restored.repoRoot,result.integrationPath);assert.equal(restored.merge.operationId,'accepted-op');
  assert.equal(restored.migrationRecovery.automatic_resume,false);
  assert.equal(await fs.readFile(path.join(restored.workspace.path,'pending.md'),'utf8'),'post-upgrade workspace delta');
  assert.equal(await fs.readFile(restored.migrationRecovery.session_files[0],'utf8'),'post-upgrade session delta');
  assert.equal(await fs.readFile(path.join(result.integrationPath,'source.md'),'utf8'),'post-upgrade integration delta');
  assert.deepEqual(JSON.parse(await fs.readFile(path.join(f.run,'run.json'),'utf8')),f.record);
  assert.equal((await verifyRecoveryBundle(original)).rollback_checkpoint,undefined);
  const status=spawnSync('jj',['--repository',result.integrationPath,'status'],{encoding:'utf8'});
  assert.equal(status.status,0,status.stderr);
  await assert.rejects(helper({operation:'export',bundle:delta,destination,projectId:'project',migrationId:'migration',rollbackCheckpoint:checkpoint}));
});

test('migration recovery callback refuses a stale project or checkpoint before creating a destination',async t=>{
  const f=await fixture(t), delta=path.join(f.root,'delta'), checkpoint=randomUUID();
  await helper({...f.identity,bundle:delta,rollbackCheckpoint:checkpoint});
  for(const changed of [{projectId:'other'},{migrationId:'other'},{rollbackCheckpoint:randomUUID()},{rollbackCheckpoint:undefined}]) {
    const destination=path.join(f.root,randomUUID());
    await assert.rejects(helper({operation:'export',bundle:delta,destination,projectId:'project',migrationId:'migration',rollbackCheckpoint:checkpoint,...changed}),/differs from its checkpoint/);
    assert.equal(existsSync(destination),false);
  }
});


test('production recovery refuses damaged retained archive bytes',async t=>{
  const f=await fixture(t), bundle=path.join(f.root,'original');
  await helper({...f.identity,bundle});
  const manifest=await verifyRecoveryBundle(bundle);
  const entry=manifest.entries.find(entry=>entry.type==='file');
  await fs.writeFile(path.join(bundle,'objects',entry.object.digest),'damaged retained data');
  const destination=path.join(f.root,'refused');
  await assert.rejects(helper({operation:'export',bundle,destination,projectId:'project',migrationId:'migration'}));
  assert.equal(existsSync(destination),false);
});
