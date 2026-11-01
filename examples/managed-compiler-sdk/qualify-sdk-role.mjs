#!/usr/bin/env node
// SOURCE ONLY / UNRUN. Trusted staging control process, not a managed VM Root
// launcher. Root deploys it at the fixed HTTPS staging origin and owns ALL
// actual Fly/PG/VM cleanup. No credential/token/config contents go to stdout.
import * as fs from 'node:fs/promises';
import {constants} from 'node:fs';
import path from 'node:path';
import {randomUUID} from 'node:crypto';
import {CompilerSdkQualificationDriver} from '../../cloud/dist/managed-home-provisioning/compiler-qualification-driver.js';

let driver;let output;let outputFailure=false;let statusBusy=false;
const retain=new Set(); // original config FD/driver/promises retained on errors
const exact=(o,names)=>{if(!o||typeof o!=='object'||Array.isArray(o)||Object.keys(o).length!==names.length||names.some(k=>!Object.hasOwn(o,k)))throw Error();};
const stamp=s=>[s.dev,s.ino,s.size,s.mode,s.mtimeNs,s.ctimeNs].map(String).join(':');
async function privateJson(file){
 if(typeof file!=='string'||!path.isAbsolute(file))throw Error();
 const f=await fs.open(file,constants.O_RDONLY|constants.O_NOFOLLOW|constants.O_CLOEXEC);retain.add(f);
 const before=await f.stat({bigint:true});if(!before.isFile()||before.uid!==BigInt(process.getuid())||before.gid!==BigInt(process.getgid())||before.nlink!==1n||(before.mode&0o7777n)!==0o600n||before.size>262144n||before.size<2n)throw Error();
 const raw=await f.readFile({encoding:'utf8'}),after=await f.stat({bigint:true}),named=await fs.lstat(file,{bigint:true});
 if(stamp(before)!==stamp(after)||stamp(before)!==stamp(named))throw Error();return JSON.parse(raw);
}
/** Private DATA output only, never a registry/Root capability constructor. */
async function writeJson(file,value){
 const bytes=Buffer.from(JSON.stringify(value,null,2)+'\n');if(bytes.length>131072)throw Error();
 const tmp=file+'.'+randomUUID()+'.tmp',f=await fs.open(tmp,constants.O_WRONLY|constants.O_CREAT|constants.O_EXCL|constants.O_NOFOLLOW,0o600);retain.add(f);
 await f.writeFile(bytes);await f.sync();await f.close();retain.delete(f);await fs.rename(tmp,file);
 const dir=await fs.open(path.dirname(file),constants.O_RDONLY|constants.O_DIRECTORY|constants.O_NOFOLLOW);retain.add(dir);await dir.sync();await dir.close();retain.delete(dir);
}
async function status(){if(!output||outputFailure||statusBusy)return;statusBusy=true;try{
 let historical=null;try{historical=await driver?.historicalResultData();}catch{}
 await writeJson(path.join(output,'status.json'),{driver:driver?.statusData()??null,historical_data:historical,physical_qualification_claim:false,root_ack_receipt_claim:false,normal_retirement_claim:false});
}catch{outputFailure=true;}finally{statusBusy=false;}}
try{
 const i=await privateJson(process.env.RUDDER_COMPILER_QUALIFICATION_INPUT);
 exact(i,['format_version','postgres','candidate','account_id','command_key']);if(i.format_version!==1)throw Error();
 exact(i.postgres,['mode','authority','registrationAuthority','policy','flyToken','tokenEncryptionKeyHex']);
 const hex=i.postgres.tokenEncryptionKeyHex;if(typeof hex!=='string'||!/^[a-f0-9]{64}$/.test(hex)||/^0+$/.test(hex))throw Error();
 const {tokenEncryptionKeyHex,...postgres}=i.postgres;postgres.tokenEncryptionKey=Buffer.from(hex,'hex');
 output=process.env.RUDDER_COMPILER_QUALIFICATION_OUTPUT;if(typeof output!=='string'||!path.isAbsolute(output))throw Error();
 await fs.mkdir(output,{mode:0o700});const dir=await fs.lstat(output);if(!dir.isDirectory()||dir.uid!==process.getuid()||dir.gid!==process.getgid()||(dir.mode&0o7777)!==0o700)throw Error();
 driver=new CompilerSdkQualificationDriver(postgres,i.candidate,i.account_id,i.command_key);retain.add(driver);
 const start=driver.startRetaining();retain.add(start);await status();start.then(status,async()=>{await status();});
 // Referenced timer + actual listening server retain original owners, including
 // failed/unknown children/provider effects, until Root's owned teardown. A
 // budget expiry closes effect admission; it never exits/restarts this process.
 setInterval(()=>{void status();},2000);
}catch{
 if(output)await writeJson(path.join(output,'failure.json'),{format_version:1,error:'qualification_driver_start_failed',owners_retained:!!driver,configured_activation:false}).catch(()=>{});
 // Static diagnostic only; never print the private input/error cause. Keep any
 // actual owner alive for Root reconciliation instead of implicit cleanup.
 process.stderr.write('Compiler SDK qualification driver requires private reconciliation\n');
 setInterval(()=>{},60000);
}
