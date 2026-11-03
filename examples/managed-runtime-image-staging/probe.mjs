// Fixed component probe only. Never mounts/formats/chowns or launches a child.
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import {constants as F} from 'node:fs';
import {createHash} from 'node:crypto';

const MOUNT='/qualification-volume', GIB=1073741824n;
const [nonce,volumeId,machineId,instanceId,probeSha256]=process.argv.slice(1);
const held=[];
let phase='arguments';
const proof={formatVersion:1,scope:'fresh-Fly-volume-filesystem-component-only',
  passed:false,nonce,volumeId,machineId,instanceId,probeSha256,
  mountPath:MOUNT,coldRestartPerformed:false,restartDurabilityQualified:false,
  encryptionVerifiedByGuest:false,physicalRootQualified:false,sourceAuthority:false,
  sdkAuthority:false,haQualified:false,fleetDurability:false,
  normalRuntimeRetirement:false,productionEnablement:false,models:false};
const hash=bytes=>createHash('sha256').update(bytes).digest('hex');
const version=st=>({dev:String(st.dev),ino:String(st.ino),uid:String(st.uid),
  gid:String(st.gid),mode:Number(st.mode&0o7777n),nlink:String(st.nlink)});
const sameDirectory=(a,b)=>a.dev===b.dev&&a.ino===b.ino&&a.uid===b.uid&&a.gid===b.gid
  &&a.mode===b.mode;
async function hold(path,flags,mode){const owner=await fs.open(path,flags,mode);held.push(owner);return owner;}
async function bounded(path,maximum){
  const owner=await hold(path,F.O_RDONLY|F.O_NOFOLLOW);
  const chunks=[];let total=0;
  for(;;){const bytes=Buffer.alloc(Math.min(4096,maximum+1-total));
    const part=await owner.read(bytes,0,bytes.length,null);
    if(part.bytesRead===0)break;
    total+=part.bytesRead;assert(total<=maximum,'bounded input exceeded');
    chunks.push(bytes.subarray(0,part.bytesRead));}
  return Buffer.concat(chunks,total);
}
function unescapeMount(value){return value.replace(/\\(040|011|012|134)/g,(_,v)=>String.fromCharCode(parseInt(v,8)));}
async function mounts(){
  const raw=(await bounded('/proc/self/mountinfo',131072)).toString('utf8');
  return raw.trim().split('\n').map(line=>{const sides=line.split(' - ');assert.equal(sides.length,2);
    const left=sides[0].split(' '),right=sides[1].split(' ');assert(left.length>=6&&right.length>=3);
    return {id:left[0],parent:left[1],device:left[2],root:unescapeMount(left[3]),
      path:unescapeMount(left[4]),options:left[5].split(','),fsType:right[0],source:unescapeMount(right[1])};});
}
function onlyMount(rows,path){const matches=rows.filter(m=>m.path===path);assert.equal(matches.length,1);return matches[0];}
function devNumbers(dev){return `${((dev>>8n)&0xfffn)|((dev>>32n)&0xfffff000n)}:${(dev&0xffn)|((dev>>12n)&0xffffff00n)}`;}
function mountedIdentity(m){return {id:m.id,parent:m.parent,device:m.device,root:m.root,path:m.path,
  options:m.options,fsType:m.fsType,source:m.source};}
async function absent(path){try{await fs.lstat(path);assert.fail('unexpected existing probe target');}
  catch(error){if(error.code!=='ENOENT')throw error;}}
async function verifiedRead(path,original,expected){
  const input=await hold(path,F.O_RDONLY|F.O_NOFOLLOW),before=await input.stat({bigint:true});
  assert(before.isFile()&&before.dev===original.dev&&before.ino===original.ino&&before.nlink===1n);
  assert.equal(before.uid,0n);assert.equal(before.gid,0n);assert.equal(before.mode&0o777n,0o600n);
  assert.equal(before.size,BigInt(expected.length));
  const bytes=Buffer.alloc(expected.length+1);let total=0;
  while(total<bytes.length){const part=await input.read(bytes,total,bytes.length-total,null);if(!part.bytesRead)break;total+=part.bytesRead;}
  assert.equal(total,expected.length);assert.deepEqual(bytes.subarray(0,total),expected);
  const after=await input.stat({bigint:true});
  for(const key of ['dev','ino','uid','gid','mode','nlink','size','mtimeNs','ctimeNs'])assert.equal(after[key],before[key]);
  return hash(bytes.subarray(0,total));
}
try{
  assert.equal(process.platform,'linux');assert.equal(process.getuid(),0);assert.equal(process.getgid(),0);
  assert.match(nonce,/^[a-f0-9]{32}$/);assert.match(volumeId,/^vol_[a-z0-9]+$/);
  assert.match(machineId,/^[a-f0-9]{14}$/);assert.match(instanceId,/^[A-Za-z0-9]{20,64}$/);
  assert.match(probeSha256,/^[a-f0-9]{64}$/);assert.equal(process.argv.slice(1).length,5);
  assert.equal(process.env.FLY_MACHINE_ID,machineId);
  phase='original-mount-and-device';
  const rows=await mounts();proof.observedMounts=rows.filter(m=>m.path==='/'||m.path===MOUNT).map(mountedIdentity);const rootMount=onlyMount(rows,'/'),mount=onlyMount(rows,MOUNT);
  assert.equal(mount.root,'/');assert.equal(mount.fsType,'ext4');assert(mount.options.includes('rw'));
  assert.notEqual(mount.id,rootMount.id);assert.notEqual(mount.device,rootMount.device);
  assert(!rows.some(m=>m.path.startsWith(MOUNT+'/')),'nested mount not part of fresh volume');
  assert.match(mount.source,/^\/dev\/[A-Za-z0-9_-]+$/);
  const root=await fs.stat('/',{bigint:true}),named=await fs.lstat(MOUNT,{bigint:true});proof.observedVolumeDirectory=version(named);
  assert(named.isDirectory()&&!named.isSymbolicLink());assert.notEqual(named.dev,root.dev);
  assert.equal(devNumbers(named.dev),mount.device);assert.equal(named.uid,0n);assert.equal(named.gid,0n);
  assert.equal(named.mode&0o022n,0n);
  const directory=await hold(MOUNT,F.O_RDONLY|F.O_DIRECTORY|F.O_NOFOLLOW);
  const original=await directory.stat({bigint:true});assert(sameDirectory(named,original));
  const anchor='/proc/self/fd/'+directory.fd;
  const device=await fs.lstat(mount.source,{bigint:true});proof.observedDevice=version(device);
  assert(device.isBlockDevice());assert.equal(device.uid,0n);assert.equal(device.mode&0o002n,0n);
  assert.equal(devNumbers(device.rdev),mount.device);
  const deviceOwner=await hold(mount.source,F.O_RDONLY|F.O_NOFOLLOW);
  const heldDevice=await deviceOwner.stat({bigint:true});
  for(const key of ['dev','ino','mode','uid','gid','rdev'])assert.equal(heldDevice[key],device[key]);
  const sectors=(await bounded('/sys/dev/block/'+mount.device+'/size',64)).toString('ascii').trim();
  assert.match(sectors,/^[0-9]{1,20}$/);const deviceBytes=BigInt(sectors)*512n;proof.observedDeviceBytes=String(deviceBytes);assert(deviceBytes>0n&&deviceBytes<=GIB,'actual guest device must fit the original nominal volume');proof.providerNominalBytes=String(GIB);
  const space=await fs.statfs(anchor,{bigint:true});assert.equal(space.type,0xef53n);
  assert(space.blocks*space.bsize>0n&&space.blocks*space.bsize<=deviceBytes);assert(space.bavail*space.bsize>=BigInt(64*1024),'actual filesystem has insufficient space for the fixed bounded probe');proof.availableFilesystemBytes=String(space.bavail*space.bsize);
  proof.rootFilesystem={...mountedIdentity(rootMount),stat:version(root)};
  proof.volumeFilesystem={...mountedIdentity(mount),stat:version(original),deviceStat:version(device),
    deviceBytes:String(deviceBytes),filesystemBytes:String(space.blocks*space.bsize),statfsType:String(space.type)};
  phase='fresh-empty-root';
  const initial=(await fs.readdir(anchor)).sort();assert(initial.length===0||initial.length===1&&initial[0]==='lost+found');
  if(initial.includes('lost+found')){
    const lost=await fs.lstat(anchor+'/lost+found',{bigint:true});assert(lost.isDirectory()&&!lost.isSymbolicLink());
    assert.equal(lost.uid,0n);assert.equal(lost.gid,0n);assert.equal(lost.dev,original.dev);assert.equal(lost.mode&0o022n,0n);
    const lostOwner=await hold(anchor+'/lost+found',F.O_RDONLY|F.O_DIRECTORY|F.O_NOFOLLOW);
    assert.equal((await lostOwner.stat({bigint:true})).ino,lost.ino);
    assert.deepEqual(await fs.readdir('/proc/self/fd/'+lostOwner.fd),[]);
  }
  proof.initialEntries=initial;proof.newEmptyExceptLostFound=true;proof.rootOwned=true;
  phase='exclusive-private-probe-directory';
  const probeName='rudder-volume-probe-'+nonce,probePath=anchor+'/'+probeName;
  await fs.mkdir(probePath,{mode:0o700});await directory.sync();
  const privateOwner=await hold(probePath,F.O_RDONLY|F.O_DIRECTORY|F.O_NOFOLLOW);
  const privateStat=await privateOwner.stat({bigint:true});
  assert(privateStat.isDirectory()&&privateStat.dev===original.dev&&privateStat.uid===0n&&privateStat.gid===0n);
  assert.equal(privateStat.mode&0o777n,0o700n);assert.deepEqual(await fs.readdir('/proc/self/fd/'+privateOwner.fd),[]);
  const privateAnchor='/proc/self/fd/'+privateOwner.fd,pending=privateAnchor+'/pending',final=privateAnchor+'/committed';
  const bytes=Buffer.concat([Buffer.from('rudder-fresh-volume-probe-v1\nnonce='+nonce+'\n','ascii'),Buffer.from(Array.from({length:4096},(_,i)=>i%251))]);
  phase='exclusive-write-and-file-directory-sync';
  const output=await hold(pending,F.O_WRONLY|F.O_CREAT|F.O_EXCL|F.O_NOFOLLOW,0o600);
  const written=await output.stat({bigint:true});assert(written.isFile()&&written.nlink===1n&&written.uid===0n&&written.gid===0n);
  assert.equal(written.dev,original.dev);assert.equal(written.mode&0o777n,0o600n);
  await output.writeFile(bytes);await output.sync();await privateOwner.sync();
  const beforeRename=await output.stat({bigint:true});assert.equal(beforeRename.size,BigInt(bytes.length));
  proof.preRenameSha256=await verifiedRead(pending,beforeRename,bytes);
  phase='same-directory-atomic-rename-and-durable-readback';
  await absent(final);await fs.rename(pending,final);await privateOwner.sync();await directory.sync();
  await absent(pending);proof.sha256=await verifiedRead(final,beforeRename,bytes);
  assert.equal(proof.preRenameSha256,proof.sha256);
  const committed=await fs.lstat(final,{bigint:true});assert.equal(committed.ino,beforeRename.ino);
  assert.deepEqual((await fs.readdir(privateAnchor)).sort(),['committed']);
  assert.deepEqual((await fs.readdir(anchor)).sort(),[...initial,probeName].sort());
  assert(sameDirectory(await fs.lstat(MOUNT,{bigint:true}),original));
  const finalDirectory=await directory.stat({bigint:true});assert(sameDirectory(finalDirectory,original));
  assert.equal(finalDirectory.nlink,original.nlink+1n);
  assert.deepEqual(mountedIdentity(onlyMount(await mounts(),MOUNT)),mountedIdentity(mount));
  const deviceAfter=await fs.lstat(mount.source,{bigint:true});
  for(const key of ['dev','ino','mode','uid','gid','rdev'])assert.equal(deviceAfter[key],device[key]);
  for(const key of ['dev','ino','mode','uid','gid','rdev'])assert.equal((await deviceOwner.stat({bigint:true}))[key],device[key]);
  Object.assign(proof,{writeBytes:bytes.length,fileStat:version(committed),probeDirectory:probeName,
    exclusiveCreate:true,noFollow:true,fileFsync:true,directoryFsync:true,atomicRename:true,
    exactReadback:true,originalMountRetained:true,passed:true});
}catch(error){proof.passed=false;proof.failure={phase,kind:error?.code==='ERR_ASSERTION'?'assertion':
  typeof error?.code==='string'&&/^[A-Z0-9_]{1,40}$/.test(error.code)?error.code:'probe-failed',message:String(error?.message??'').slice(0,300),stack:String(error?.stack??'').split('\n').slice(0,3).join('\n')};}
// Explicit awaited handle closure only; persistent files stay until this exact
// owned volume is destroyed. No filesystem deletion or managed drain claim.
let closed=true;
for(const handle of held.reverse())try{await handle.close();}catch{closed=false;}
proof.ownedFileHandlesClosed=closed;proof.passed&&=closed;
process.stdout.write(JSON.stringify(proof)+'\n');process.exitCode=proof.passed?0:1;
