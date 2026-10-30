import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {createReadStream} from 'node:fs';
import fs from 'node:fs/promises';
import path from 'node:path';
import {Readable} from 'node:stream';
import test from 'node:test';
import {withVerifiedSpool, checkedBlobStream} from '../cloud/dist/fabric-stream.js';
import {S3FabricObjectStorage} from '../cloud/dist/fabric-cas.js';

const reference=(bytes)=>({kind:'blob',algorithm:'sha256',digest:createHash('sha256').update(bytes).digest('hex'),size_bytes:bytes.length});
async function digest(body){const hash=createHash('sha256');let size=0;for await(const chunk of body){hash.update(chunk);size+=chunk.length;}return{digest:hash.digest('hex'),size};}

test('a compiler-sized blob is privately spooled and streamed without full heap buffering',async()=>{
  const chunk=Buffer.alloc(64*1024,0x53),count=1120;
  const hash=createHash('sha256');for(let n=0;n<count;n++)hash.update(chunk);
  const ref={kind:'blob',algorithm:'sha256',digest:hash.digest('hex'),size_bytes:chunk.length*count};
  let file;
  const result=await withVerifiedSpool(ref,(async function*(){for(let n=0;n<count;n++)yield chunk;})(),async(location)=>{
    file=location;
    assert.equal((await fs.stat(location)).mode&0o777,0o600);
    assert.equal((await fs.stat(path.dirname(location))).mode&0o777,0o700);
    return await digest(createReadStream(location,{highWaterMark:64*1024}));
  });
  assert.equal(result.digest,ref.digest);assert.equal(result.size,ref.size_bytes);
  await assert.rejects(fs.stat(file),{code:'ENOENT'});
});

test('short, oversized and corrupted bodies never reach immutable storage',async()=>{
  const ref=reference(Buffer.from('expected'));
  for(const bytes of [Buffer.from('expecte'),Buffer.from('expected-too-long'),Buffer.from('corrupt!')]){
    let called=false;
    await assert.rejects(withVerifiedSpool(ref,Readable.from([bytes]),async()=>{called=true;}));
    assert.equal(called,false);
  }
});

test('spools clean up after uncertain storage outcomes and body deadlines',async()=>{
  let file;const bytes=Buffer.from('verified');
  await assert.rejects(withVerifiedSpool(reference(bytes),Readable.from([bytes]),async(location)=>{file=location;throw new Error('private transport failure');}));
  await assert.rejects(fs.stat(file),{code:'ENOENT'});
  const source=new Readable({read(){}});
  await assert.rejects(withVerifiedSpool(reference(bytes),source,async()=>assert.fail('late storage write'),10),{code:'body_timeout'});
  assert.equal(source.destroyed,true);
});

test('partial consumption and corruption cannot become verification evidence',async()=>{
  const bytes=Buffer.from('hello');const ref=reference(bytes);
  await assert.rejects(checkedBlobStream(ref,Readable.from([bytes]),async(body)=>{for await(const chunk of body)break;}),{code:'incomplete_read'});
  await assert.rejects(checkedBlobStream(ref,Readable.from([Buffer.from('bad!!')]),async(body)=>{for await(const chunk of body){}}),{code:'checksum_mismatch'});
  await checkedBlobStream(ref,Readable.from([bytes]),async(body)=>{assert.equal((await digest(body)).digest,ref.digest);});
});

test('S3 file uploads require conditional writes and the preverified SHA256 header',async()=>{
  const bytes=Buffer.from('payload'),ref=reference(bytes);let body;
  const storage=new S3FabricObjectStorage({bucket:'private-fixture',client:{async send(command){
    assert.equal(command.input.IfNoneMatch,'*');assert.equal(command.input.ContentLength,bytes.length);
    assert.equal(command.input.ChecksumSHA256,Buffer.from(ref.digest,'hex').toString('base64'));
    assert.equal(command.input.ServerSideEncryption,'AES256');body=command.input.Body;
    assert.equal((await digest(body)).digest,ref.digest);return {};
  }}});
  await withVerifiedSpool(ref,Readable.from([bytes]),(file)=>storage.putFileIfAbsent('content',file,ref));
  assert.equal(body.destroyed,true);
});

test('S3 stream read deadlines destroy a stalled response and preserve the limit',async()=>{
  const body=new Readable({read(){}});
  const storage=new S3FabricObjectStorage({bucket:'fixture',timeoutMs:15,maxConcurrency:1,client:{async send(){return{Body:body,ETag:'fixture',ContentLength:10};}}});
  await assert.rejects(storage.readStream('content',10,async(source)=>{for await(const chunk of source){}}),{code:'storage_timeout'});
  assert.equal(body.destroyed,true);
});

test('metadata read deadlines release acquired bodies and restore storage admission',async()=>{
  const stalled=new Readable({read(){}});let calls=0;
  const storage=new S3FabricObjectStorage({bucket:'fixture',timeoutMs:15,maxConcurrency:1,client:{async send(){
    return {Body:++calls===1?stalled:Readable.from([Buffer.from('ok')]),ETag:'fixture',ContentLength:2};
  }}});
  await assert.rejects(storage.get('metadata',2),{code:'storage_timeout'});
  assert.equal(stalled.destroyed,true);
  await new Promise(setImmediate);
  assert.equal((await storage.get('metadata',2)).body.toString(),'ok');
});

test('startup spool cleanup removes only dead private owners and retains live uploads',async()=>{
  const {tmpdir}=await import('node:os');
  const {cleanupFabricUploadSpools}=await import('../cloud/dist/fabric-stream.js');
  const dead=await fs.mkdtemp(path.join(tmpdir(),'rudder-fabric-upload-2147483647-'));
  const live=await fs.mkdtemp(path.join(tmpdir(),`rudder-fabric-upload-${process.pid}-`));
  const unrelated=await fs.mkdtemp(path.join(tmpdir(),'unrelated-uploads-'));
  try{
    for(const dir of [dead,live,unrelated])await fs.writeFile(path.join(dir,'content'),'private upload');
    await cleanupFabricUploadSpools();
    await assert.rejects(fs.stat(dead),{code:'ENOENT'});
    assert((await fs.stat(live)).isDirectory());assert((await fs.stat(unrelated)).isDirectory());
  }finally{for(const dir of [dead,live,unrelated])await fs.rm(dir,{recursive:true,force:true});}
});
