// Read-only local build evidence. Import/default performs no IO or processes.
import fs from 'node:fs/promises';
import {constants} from 'node:fs';
import path from 'node:path';
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';

const hash = bytes => createHash('sha256').update(bytes).digest('hex');
const sorted = value => Array.isArray(value)?value.map(sorted):value&&typeof value==='object'
  ?Object.fromEntries(Object.keys(value).sort().map(key=>[key,sorted(value[key])])):value;
const parent = 'sha256:53c763d9e5886cadc262fa03631e5d08338c44486e2f3ffbb8e0afba4c21d89f';
const expectedCommand = ['cargo','build','--release','--workspace','--locked','--features','rudder-vcs-bridge/cli',
  '--bin','rudder-native','--bin','rudder-jj','--bin','rudder-connected-network-qualifier'];

export async function stableFile(file, maximum, retain = true) {
  assert(path.isAbsolute(file));
  const handle = await fs.open(file, constants.O_RDONLY|constants.O_NOFOLLOW|constants.O_NONBLOCK);
  try {
    const before = await handle.stat();
    assert(before.isFile() && before.size >= 0 && before.size <= maximum);
    const digest = createHash('sha256'), pieces = [], buffer = Buffer.alloc(65536);
    let prefix = Buffer.alloc(0);
    let size = 0;
    for (;;) {
      const answer = await handle.read(buffer,0,buffer.length,null);
      if (!answer.bytesRead) break;
      size += answer.bytesRead; assert(size <= maximum && size <= before.size);
      if (prefix.length<64) prefix=Buffer.concat([prefix,buffer.subarray(0,Math.min(answer.bytesRead,64-prefix.length))]);
      digest.update(buffer.subarray(0,answer.bytesRead));
      if (retain) pieces.push(Buffer.from(buffer.subarray(0,answer.bytesRead)));
    }
    assert.equal(size,before.size);
    for (const next of [await handle.stat(),await fs.lstat(file)]) {
      assert(next.isFile() && !next.isSymbolicLink());
      for (const field of ['dev','ino','size','mtimeMs','mode']) assert.equal(next[field],before[field]);
    }
    return {sha256:digest.digest('hex'),bytes:size,mode:before.mode&0o7777,prefix,...(retain?{body:Buffer.concat(pieces)}:{})};
  } finally {await handle.close();}
}

export async function loadBinding(file) {
  const read = await stableFile(path.resolve(file),1024*1024), binding = JSON.parse(read.body);
  assert.equal(binding.formatVersion,1);assert.equal(binding.kind,'alpha22-connect-canary-artifact-binding-v1');
  assert.equal(binding.parentDigest,parent);assert.equal(binding.sourceKind,'frozen-working-tree');
  assert.match(binding.image,/^registry\.fly\.io\/rudder-workers-exla:alpha22-connect-canary-[A-Za-z0-9_.-]{1,80}$/);
  assert.match(binding.digest,/^sha256:[0-9a-f]{64}$/);assert.notEqual(binding.digest,parent);
  for (const key of ['sourceEnvelopeSha256','sourceTreeSha256','compilerReceiptSha256','nativeSha256',
    'qualifierSha256','buildMetadataSha256','dockerfileSha256']) assert.match(binding[key],/^[0-9a-f]{64}$/);
  assert.match(binding.revision,/^[0-9a-f]{40}$/);
  assert(path.isAbsolute(binding.directory));assert.equal(await fs.realpath(binding.directory),binding.directory);
  return {binding,sha256:read.sha256};
}

export function artifactArgv(binding, bindingFile, bindingSha256, {nonce,host} = {}) {
  const fields = {image:binding.image,digest:binding.digest,'parent-digest':binding.parentDigest,
    'qualifier-sha256':binding.qualifierSha256,'native-sha256':binding.nativeSha256,
    'source-envelope-sha256':binding.sourceEnvelopeSha256,'source-tree-sha256':binding.sourceTreeSha256,
    'compiler-receipt-sha256':binding.compilerReceiptSha256,revision:binding.revision,
    'source-kind':binding.sourceKind,'artifact-binding':path.resolve(bindingFile),'artifact-binding-sha256':bindingSha256};
  return ['--run',...Object.entries(fields).flatMap(([key,value])=>['--'+key,value]),
    ...(nonce?['--nonce',nonce]:[]),...(host?['--host',host]:[])];
}

export async function verifyArtifactInputs(value) {
  const {binding,sha256} = await loadBinding(value.artifactBinding);
  assert.equal(sha256,value.artifactBindingSha256);
  for (const key of ['image','digest','parentDigest','qualifierSha256','nativeSha256','sourceEnvelopeSha256',
    'sourceTreeSha256','compilerReceiptSha256','revision','sourceKind']) assert.equal(binding[key],value[key]);
  const root = binding.directory;
  const envelopeFile = await stableFile(path.join(root,'source-envelope.json'),16*1024*1024);
  assert.equal(envelopeFile.sha256,binding.sourceEnvelopeSha256);
  const envelope = JSON.parse(envelopeFile.body);
  assert.equal(envelope.kind,'frozen-working-tree-compiler-inputs-v1');
  assert.equal(envelope.sourceKind,'frozen-working-tree');assert.equal(envelope.baseRevision,binding.revision);
  assert.equal(envelope.sourceTreeSha256,binding.sourceTreeSha256);
  const manifestText=JSON.stringify(sorted(envelope.files)).replace(/[\u007f-\uffff]/g,
    character=>'\\u'+character.charCodeAt(0).toString(16).padStart(4,'0'))+'\n';
  assert.equal(hash(Buffer.from(manifestText)),binding.sourceTreeSha256);
  assert.equal(envelope.baseRevisionIsCallerDeclared,true);
  assert(Object.keys(envelope.files).length <= 10000);
  let total = 0;
  for (const [relative,record] of Object.entries(envelope.files)) {
    assert(!path.isAbsolute(relative) && relative.split('/').every(part=>part && part!=='.' && part!=='..'));
    assert(/^(?:(?:Cargo\.(?:toml|lock)|package\.json|rust-toolchain(?:\.toml)?|\.cargo\/config(?:\.toml)?)$|(?:native|engine|vcs-bridge|assets)\/)/.test(relative));
    const source = await stableFile(path.join(root,'source',relative),32*1024*1024,false);
    assert.equal(source.sha256,record.sha256);assert.equal(source.bytes,record.bytes);
    assert.equal(source.mode,record.mode);
    total += source.bytes; assert(total <= 256*1024*1024);
  }
  assert.equal(total,envelope.sourceBytes);assert.equal(Object.keys(envelope.files).length,envelope.sourceFiles);
  for (const key of ['engine/src/bin/connected_network_qualifier.rs','engine/src/execution/network/connected/qualification.rs',
    'engine/src/execution/network/connected/validity.rs','engine/src/execution/network/connected/relay.rs',
    'native/src/managed_backend_prepare.rs']) assert(Object.hasOwn(envelope.files,key));
  const receiptFile = await stableFile(path.join(root,'compiler-receipt.json'),1024*1024);
  assert.equal(receiptFile.sha256,binding.compilerReceiptSha256);
  const receipt = JSON.parse(receiptFile.body);
  assert.equal(receipt.kind,'actual-cargo-release-compiler-receipt-v1');assert.equal(receipt.ok,true);
  assert.equal(receipt.sourceKind,'frozen-working-tree');
  assert.equal(receipt.exitCode,0);assert.equal(receipt.sourceUnchanged,true);
  assert.equal(receipt.compilerProcessReaped,true);assert.equal(receipt.guardReason,null);
  assert.deepEqual(receipt.argv,expectedCommand);assert.equal(receipt.sourceEnvelopeSha256,binding.sourceEnvelopeSha256);
  assert.equal(receipt.sourceTreeSha256,binding.sourceTreeSha256);
  for (const [name,expected] of [['rudder-native',binding.nativeSha256],['rudder-connected-network-qualifier',binding.qualifierSha256]]) {
    const record = receipt.artifacts[name];assert.equal(record.sha256,expected);assert.equal(record.elfMachine,'EM_X86_64');
    assert.equal(record.path,path.join(root,'target/release',name));
    const actual = await stableFile(record.path,128*1024*1024,false);
    assert.equal(actual.sha256,expected);assert.equal(actual.bytes,record.bytes);
    assert.equal(actual.prefix.subarray(0,6).toString('hex'),'7f454c460201');
    assert.equal(actual.prefix.subarray(18,20).toString('hex'),'3e00');
  }
  assert.equal(receipt.logPath,path.join(root,'compiler.private.log'));
  assert.equal((await stableFile(receipt.logPath,64*1024*1024,false)).sha256,receipt.logSha256);
  const metadata = await stableFile(binding.buildMetadataPath,1024*1024);
  assert.equal(metadata.sha256,binding.buildMetadataSha256);
  assert.equal(JSON.parse(metadata.body)['containerimage.digest'],binding.digest);
  const dockerfile = await stableFile(path.join(root,'image/Dockerfile'),8192);
  assert.equal(dockerfile.sha256,binding.dockerfileSha256);
  assert.equal(dockerfile.body.toString().split('\n')[0],'FROM registry.fly.io/rudder-workers-exla@'+parent);
  assert.equal((await stableFile(path.join(root,'image/rudder-connected-network-qualifier'),128*1024*1024,false)).sha256,binding.qualifierSha256);
  const imageProvenance = JSON.parse((await stableFile(path.join(root,'image/provenance.json'),8192)).body);
  assert.deepEqual(imageProvenance,binding.provenance);
  return {verified:true,artifactBindingSha256:sha256,sourceEnvelopeSha256:binding.sourceEnvelopeSha256,
    sourceTreeSha256:binding.sourceTreeSha256,compilerReceiptSha256:binding.compilerReceiptSha256,
    nativeSha256:binding.nativeSha256,qualifierSha256:binding.qualifierSha256,
    qualifierCallerSourceSha256:envelope.files['engine/src/execution/network/connected/qualification.rs'].sha256,
    validitySourceSha256:envelope.files['engine/src/execution/network/connected/validity.rs'].sha256,
    sourceFiles:envelope.sourceFiles,sourceBytes:total,nativeBinaryExecuted:false};
}
