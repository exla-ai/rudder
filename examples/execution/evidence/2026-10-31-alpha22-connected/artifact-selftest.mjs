// Synthetic local files only. No compiler, process spawn, /proc, DNS or provider.
import fs from 'node:fs/promises';
import path from 'node:path';
import {fileURLToPath} from 'node:url';
import {createHash} from 'node:crypto';
import assert from 'node:assert/strict';
import {artifactArgv,loadBinding,verifyArtifactInputs} from './artifact-inputs.mjs';
import {options,provenanceFor} from './qualify-connected-network.mjs';
import {run} from './run-canary.mjs';

const base=path.dirname(fileURLToPath(import.meta.url)),directory=await fs.mkdtemp(path.join(base,'mock-artifacts-'));
const hash=body=>createHash('sha256').update(body).digest('hex');
const sorted=value=>value&&typeof value==='object'&&!Array.isArray(value)
  ?Object.fromEntries(Object.keys(value).sort().map(key=>[key,sorted(value[key])])):value;
const encode=value=>Buffer.from(JSON.stringify(sorted(value))+'\n');
const write=async(name,body)=>{const target=path.join(directory,name);await fs.mkdir(path.dirname(target),{recursive:true,mode:0o700});
  await fs.writeFile(target,body,{mode:0o600,flag:'wx'});return target;};
try {
  assert.equal((await run([])).providerCalls,0);assert.equal((await run(['--describe'])).providerCalls,0);
  const sources=['Cargo.toml','Cargo.lock','package.json','engine/src/bin/connected_network_qualifier.rs',
    'engine/src/execution/network/connected/qualification.rs','engine/src/execution/network/connected/validity.rs',
    'engine/src/execution/network/connected/relay.rs','native/src/managed_backend_prepare.rs'];
  const files={};let sourceBytes=0;
  for (const name of sources) {const body=Buffer.from('synthetic mock compiler input; never compiled '+name+'\n');
    const file=await write('source/'+name,body);await fs.chmod(file,0o644);
    files[name]={sha256:hash(body),bytes:body.length,mode:0o644};sourceBytes+=body.length;}
  const envelope={formatVersion:1,kind:'frozen-working-tree-compiler-inputs-v1',sourceKind:'frozen-working-tree',
    baseRevision:'1'.repeat(40),baseRevisionIsCallerDeclared:true,sourceTreeSha256:hash(encode(files)),
    sourceFiles:sources.length,sourceBytes,files};
  const envelopeBytes=encode(envelope);await write('source-envelope.json',envelopeBytes);
  const artifacts={};
  for (const name of ['rudder-native','rudder-jj','rudder-connected-network-qualifier']) {
    const body=Buffer.alloc(128);Buffer.from('synthetic ELF-shaped artifact, never executable '+name).copy(body,32);
    Buffer.from('7f454c460201','hex').copy(body);body[18]=0x3e;
    const location=await write('target/release/'+name,body);
    artifacts[name]={path:location,sha256:hash(body),bytes:body.length,elfMachine:'EM_X86_64'};
  }
  const log=Buffer.from('synthetic mock log; compiler was not invoked\n');
  const logPath=await write('compiler.private.log',log);
  const receipt={formatVersion:1,kind:'actual-cargo-release-compiler-receipt-v1',ok:true,exitCode:0,sourceUnchanged:true,
    sourceKind:'frozen-working-tree',
    compilerProcessReaped:true,guardReason:null,
    argv:['cargo','build','--release','--workspace','--locked','--features','rudder-vcs-bridge/cli','--bin','rudder-native',
      '--bin','rudder-jj','--bin','rudder-connected-network-qualifier'],sourceEnvelopeSha256:hash(envelopeBytes),
    sourceTreeSha256:envelope.sourceTreeSha256,logPath,logSha256:hash(log),artifacts};
  const receiptBytes=encode(receipt);await write('compiler-receipt.json',receiptBytes);
  const parentDigest='sha256:53c763d9e5886cadc262fa03631e5d08338c44486e2f3ffbb8e0afba4c21d89f';
  const metadataBytes=encode({'containerimage.digest':'sha256:'+'2'.repeat(64)});
  const metadataPath=await write('image-build-metadata.json',metadataBytes);
  const dockerfile=Buffer.from('FROM registry.fly.io/rudder-workers-exla@'+parentDigest+'\n');
  await write('image/Dockerfile',dockerfile);
  await write('image/rudder-connected-network-qualifier',await fs.readFile(artifacts['rudder-connected-network-qualifier'].path));
  const binding={formatVersion:1,kind:'alpha22-connect-canary-artifact-binding-v1',directory,
    image:'registry.fly.io/rudder-workers-exla:alpha22-connect-canary-mock-artifacts',digest:'sha256:'+'2'.repeat(64),
    parentDigest,sourceKind:'frozen-working-tree',revision:envelope.baseRevision,sourceEnvelopeSha256:hash(envelopeBytes),
    sourceTreeSha256:envelope.sourceTreeSha256,compilerReceiptSha256:hash(receiptBytes),
    nativeSha256:artifacts['rudder-native'].sha256,qualifierSha256:artifacts['rudder-connected-network-qualifier'].sha256,
    buildMetadataPath:metadataPath,buildMetadataSha256:hash(metadataBytes),dockerfileSha256:hash(dockerfile)};
  binding.provenance=provenanceFor(binding);
  await write('image/provenance.json',encode(binding.provenance));
  const bindingFile=await write('run-binding.json',encode(binding));
  const loaded=await loadBinding(bindingFile);
  const value=options(artifactArgv(loaded.binding,bindingFile,loaded.sha256));
  const answer=await verifyArtifactInputs(value);
  assert.equal(answer.verified,true);assert.equal(answer.nativeBinaryExecuted,false);
  let rejections=0;
  for (const name of ['source/engine/src/execution/network/connected/validity.rs',
    'target/release/rudder-native','target/release/rudder-connected-network-qualifier','compiler.private.log',
    'image-build-metadata.json','image/Dockerfile','image/provenance.json','image/rudder-connected-network-qualifier']) {
    const target=path.join(directory,name),saved=await fs.readFile(target);
    await fs.writeFile(target,Buffer.from('changed synthetic mock input\n'));
    await assert.rejects(()=>verifyArtifactInputs(value));rejections++;await fs.writeFile(target,saved);
  }
  await assert.rejects(()=>verifyArtifactInputs({...value,compilerReceiptSha256:'f'.repeat(64)}));rejections++;
  console.log(JSON.stringify({kind:'alpha22-connect-artifact-selftest',testOnlyMock:true,
    sourceBinaryReceiptImageBindingsChecked:true,changedInputRejections:rejections,
    providerCalls:0,dnsCalls:0,compilerCommands:0,actualArtifactsQualified:false}));
} finally {await fs.rm(directory,{recursive:true,force:true});}
