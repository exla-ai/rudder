// PRIVATE qualification-only transports. Never imported by Cloud production.
import assert from 'node:assert/strict';
import {createHash,randomUUID} from 'node:crypto';
import {Readable} from 'node:stream';
import {createRequire} from 'node:module';
export const hash=b=>createHash('sha256').update(b).digest('hex');
let imageHardlinks=false,graphsCreated=0,immutableBatchRetention=false,storageCreated=0;
export function selectImmutableBatchRetentionFixture(){assert.equal(storageCreated,0);assert.equal(immutableBatchRetention,false);immutableBatchRetention=true;}
// Explicit hash-bound test module selects this BEFORE policy/intent creation.
// This fixture-only switch cannot change any production policy or authority.
export function selectHardlinkImageFixture(){assert.equal(graphsCreated,0);assert.equal(imageHardlinks,false);imageHardlinks=true;}

// Actual S3FabricObjectStorage, actual FabricCas, fake conditional S3 service.
// No subclass/instanceof bypass, custom PG pool or external network call.
export function memoryS3(S3FabricObjectStorage){storageCreated++;
  let heldGate;const controls={holdNext(match){assert.equal(typeof match,'function');assert(!heldGate);let enter,release;const entered=new Promise(r=>{enter=r;}),settle=new Promise(r=>{release=r;});heldGate={match,enter,settle,claimed:false};return {entered,release:()=>release()};}};
  const objects=new Map(),successfulGets=new Map(),stats={get:0,put:0,delete:0,bytes:0};
  controls.objectBodyReads=(account,reference)=>{assert.equal(reference.algorithm,'sha256');return successfulGets.get('fabric/v1/accounts/'+hash(Buffer.from(account))+'/objects/sha256/'+reference.digest)||0;};
  const failure=status=>Object.assign(new Error('fixture object operation'),{$metadata:{httpStatusCode:status}});
  const client={async send(command,options={}){
    assert(!options.abortSignal?.aborted);const p=command.input,key=p.Key;if(heldGate&&!heldGate.claimed&&heldGate.match({operation:command.constructor.name,key})){const held=heldGate;held.claimed=true;held.enter();await held.settle;assert.equal(heldGate,held);heldGate=undefined;}assert.equal(p.Bucket,'managed-pg-fixture');
    if(immutableBatchRetention){
      if(command.constructor.name==='GetBucketVersioningCommand'){stats.retentionGet++;return {Status:'Enabled'};}
      if(command.constructor.name==='GetBucketLifecycleConfigurationCommand'){stats.retentionGet++;return {Rules:[]};}
      if(command.constructor.name==='GetBucketPolicyCommand'){stats.retentionGet++;return {Policy:JSON.stringify(fixturePolicy)};}
    }
    assert.equal(typeof key,'string');
    switch(command.constructor.name){
      case 'GetObjectCommand':{stats.get++;if(immutableBatchRetention&&key.includes('/staging/'))stats.leaseGet++;const v=objects.get(key);if(!v)throw failure(404);successfulGets.set(key,(successfulGets.get(key)||0)+1);return {Body:Readable.from([Buffer.from(v.bytes)]),ETag:v.etag,ContentLength:v.bytes.length};}
      case 'PutObjectCommand':{stats.put++;if(immutableBatchRetention&&key.includes('/batch-holds-v1/'))stats.holdPut++;const old=objects.get(key);if(p.IfNoneMatch==='*'&&old)throw failure(412);if(p.IfMatch!==undefined&&old?.etag!==p.IfMatch)throw failure(412);
        assert(p.IfNoneMatch==='*'||typeof p.IfMatch==='string');const parts=[];let total=0;
        if(Buffer.isBuffer(p.Body)||p.Body instanceof Uint8Array){parts.push(Buffer.from(p.Body));total=p.Body.length;}
        else for await(const b of p.Body){assert(!options.abortSignal?.aborted);total+=b.length;assert(total<=8*1024*1024);parts.push(Buffer.from(b));}
        const bytes=Buffer.concat(parts);assert.equal(total,p.ContentLength);assert.equal(createHash('sha256').update(bytes).digest('base64'),p.ChecksumSHA256);
        assert(objects.size<2048||old);stats.bytes+=bytes.length;assert(stats.bytes<=32*1024*1024);objects.set(key,{bytes,etag:'"'+hash(bytes)+'"'});return {ETag:objects.get(key).etag};}
      case 'DeleteObjectCommand':stats.delete++;objects.delete(key);return {};
      default:throw Error('unconfigured fixture S3 command');
    }
  }};
  let storage;
  const fixtureRole='arn:aws:iam::111122223333:role/rudder-managed-pg-fixture',root='arn:aws:s3:::managed-pg-fixture',resources=['objects/*','roots/*','batch-holds-v1/*'].map(p=>root+'/fabric/v1/accounts/*/'+p);
  const fixturePolicy={Version:'2012-10-17',Statement:[{Effect:'Deny',Principal:'*',Action:'s3:PutObject',Resource:resources,Condition:{Null:{'s3:if-none-match':'true'},Bool:{'s3:ObjectCreationOperation':'true'}}},{Effect:'Deny',Principal:'*',Action:['s3:DeleteObject','s3:DeleteObjectVersion'],Resource:resources,Condition:{ArnEquals:{'aws:PrincipalArn':fixtureRole}}},{Effect:'Deny',Principal:'*',Action:['s3:PutLifecycleConfiguration','s3:PutBucketVersioning','s3:PutBucketPolicy','s3:DeleteBucketPolicy','s3:DeleteBucket'],Resource:root,Condition:{ArnEquals:{'aws:PrincipalArn':fixtureRole}}}]};
  if(immutableBatchRetention){
    Object.assign(stats,{retentionGet:0,leaseGet:0,holdPut:0});controls.hasHold=(account,id)=>objects.has('fabric/v1/accounts/'+hash(Buffer.from(account))+'/batch-holds-v1/'+id+'.json');
    const require=createRequire(new URL('../cloud/package.json',import.meta.url)),{S3Client}=require('@aws-sdk/client-s3');
    // Isolated fixture process only: never resolve credentials/read the token
    // path or send to AWS. Unknown commands/buckets fail closed. Kept until VM
    // teardown with unknown original owners; this is NOT production admission.
    S3Client.prototype.send=function(command,options){assert.equal(command.input.Bucket,'managed-pg-fixture');return client.send(command,options);};
    storage=S3FabricObjectStorage.fromWebIdentityForImmutableBatches({bucket:'managed-pg-fixture',region:'us-east-1',roleArn:fixtureRole,webIdentityTokenFile:'/var/tmp/THIS_FIXTURE_NEVER_READS_A_TOKEN',timeoutMs:30000,maxConcurrency:4});
  }else storage=new S3FabricObjectStorage({client,bucket:'managed-pg-fixture',timeoutMs:30000,maxConcurrency:4});
  return {storage,stats,controls,
    evidence:{scope:'actual-FabricCas-and-S3-adapter-with-in-memory-conditional-service',actualS3:false,HA:false,RPO:false,...(immutableBatchRetention?{realWorkloadRole:false,syntheticPolicyReadback:true}: {})}};
}

export function flyFetchFixture(){
  const apps=new Map(),counts={app:0,volume:0,machine:0,secret:0,configure:0,start:0,lease:0,release:0,read:0};
  const fetch=async(url,options)=>{
    const u=new URL(url);assert.equal(u.origin,'https://api.machines.dev');assert.equal(options.redirect,'error');assert.equal(options.headers.Authorization,'Bearer synthetic-managed-pg-provider-only');
    assert(!options.signal.aborted);const method=options.method,body=options.body?JSON.parse(options.body):undefined;let result,status=200;
    if(method==='POST'&&u.pathname==='/v1/apps'){
      counts.app++;assert.equal(u.search,'?wait=true');assert(!apps.has(body.name));
      const app={id:'fixture-app-'+randomUUID(),name:body.name,network:body.network,network_cidr:'fd00:1234::/64',organization:{slug:body.org_slug}};
      apps.set(body.name,{app,secrets:new Map()});result=app;status=201;
    }else{
      const m=/^\/v1\/apps\/([^/]+)(.*)$/.exec(u.pathname);assert(m);const a=apps.get(m[1]);assert(a);const suffix=m[2];
      if(method==='GET'&&suffix===''){counts.read++;result=a.app;}
      else if(method==='POST'&&suffix==='/volumes'){counts.volume++;assert.equal(body.encrypted,true);a.volume={id:'fixture-volume-'+randomUUID(),name:body.name,region:body.region,size_gb:body.size_gb};result=a.volume;}
      else if(method==='POST'&&suffix==='/machines'){counts.machine++;assert(body.skip_launch&&body.skip_secrets&&body.skip_service_registration);assert(!a.machine);
        a.machine={id:'fixture-machine-'+randomUUID(),instance_id:'fixture-created-'+randomUUID(),name:body.name,region:body.region,state:'created',host_status:'ok',config:structuredClone(body.config)};result=a.machine;status=201;}
      else if(method==='POST'&&suffix.startsWith('/secrets/')){counts.secret++;const name=suffix.slice('/secrets/'.length);assert.equal(typeof body.value,'string');a.secrets.set(name,body.value);result={name,digest:hash(body.value),version:1};}
      else {
        const n=/^\/machines\/([^/]+)(.*)$/.exec(suffix);assert(n&&a.machine?.id===n[1]);const tail=n[2];
        if(method==='GET'&&tail===''){counts.read++;result=a.machine;}
        else if(method==='POST'&&tail==='/lease'){counts.lease++;assert(!a.lease);a.lease={nonce:randomUUID(),version:a.machine.instance_id,description:body.description,owner:'fixture-provider',expires_at:Math.floor(Date.now()/1000)+15};result={status:'success',data:a.lease};status=201;}
        else if(method==='GET'&&tail==='/lease'){counts.read++;assert(a.lease);result={status:'success',data:a.lease};}
        else if(method==='DELETE'&&tail==='/lease'){counts.release++;assert.equal(options.headers['fly-machine-lease-nonce'],a.lease.nonce);a.lease=undefined;result={status:'success',data:{ok:true}};}
        else if(method==='POST'&&tail===''){counts.configure++;assert.equal(options.headers['fly-machine-lease-nonce'],a.lease.nonce);assert.equal(body.current_version,a.machine.instance_id);assert(body.skip_launch&&body.skip_service_registration);
          a.machine={...a.machine,config:structuredClone(body.config),instance_id:'fixture-configured-'+randomUUID(),state:'stopped'};result=a.machine;}
        else if(method==='POST'&&tail==='/start'){counts.start++;assert.equal(options.headers['fly-machine-lease-nonce'],a.lease.nonce);a.machine.state='started';result={};}
        else throw Error('unconfigured fixture Fly request');
      }
    }
    return new Response(JSON.stringify(result),{status,headers:{'Content-Type':'application/json'}});
  };
  const forHome=home=>[...apps.values()].find(a=>a.machine?.name===home);
  return {fetch,counts,forHome};
}

export function componentGraph(canonicalFabricMetadata){
  graphsCreated++;
  const all=new Map();
  const add=(kind,bytes)=>{const ref={kind,algorithm:'sha256',digest:hash(bytes),size_bytes:bytes.length};all.set(kind+':'+ref.digest,{ref,bytes});return ref;};
  const blob=text=>add('blob',Buffer.from(text));
  const meta=(kind,value)=>add(kind,canonicalFabricMetadata(value));
  const contents=[['/opt/rudder-managed-runtime/v1/codex',blob('fixture codex bytes')],['/opt/rudder/dist/native/rudder-native',blob('fixture native bytes')],['/usr/local/bin/node',blob('fixture node bytes')]];
  let hardlinks;
  if(imageHardlinks){
    const bytes=Buffer.alloc(4*1024*1024,0x67),content=add('blob',bytes),aliases=513;
    for(let i=0;i<aliases;i++)contents.push(['/usr/local/libexec/git-core/git-hardlink-fixture-'+String(i).padStart(4,'0'),content]);
    hardlinks={content,aliases,logicalAliasBytes:aliases*bytes.length};assert(hardlinks.logicalAliasBytes>2*1024**3);
  }
  const installer=meta('directory',{format_version:1,mode:493,entries:contents.map(([_,object],i)=>({name_hex:Buffer.from('file'+(imageHardlinks?String(i).padStart(6,'0'):i)).toString('hex'),node:{type:'file',object,mode:365}}))});
  const files=contents.map(([path,content])=>({path,sha256:content.digest,size_bytes:content.size_bytes,content})).sort((a,b)=>Buffer.compare(Buffer.from(a.path),Buffer.from(b.path)));
  const manifest=blob(canonicalFabricMetadata({format_version:1,installer_input:installer,files}).toString());
  const imageObjects=[...all.values()];const empty=meta('directory',{format_version:1,mode:493,entries:[]});const policyBlob=blob('{"fixture":"source-only"}');
  const source=meta('source_root',{format_version:2,consistency:'quiesced',directory:empty,projection_policy:policyBlob});
  const environment=meta('environment_root',{format_version:2,directory:empty,platform:{os:'linux',arch:'x86_64'},installer_inputs:[installer],provenance:{qualification:'fixture-only'}});
  const image={manifest,installer,objects:imageObjects,files,...(hardlinks?{hardlinks:{...hardlinks,logicalPathBytes:files.reduce((n,f)=>n+f.size_bytes,0),uniqueGraphBytes:imageObjects.reduce((n,o)=>n+o.ref.size_bytes,0)}}:{})};
  if(hardlinks){assert(Number.isSafeInteger(image.hardlinks.logicalPathBytes));assert(image.hardlinks.uniqueGraphBytes<2*1024**3);}
  const p={organization:'managed-pg-fixture',region:'iad',image:'registry.fly.io/rudder-workers-exla@sha256:'+hash('fixture-image'),runtimeManifest:{profile_id:'fly-managed-image-v1',manifest_sha256:hash('fixture runtime manifest'),entries:[{path:contents[0][0],sha256:contents[0][1].digest,size_bytes:contents[0][1].size_bytes}]},
    imagePreseed:{fullManifestSha256:manifest.digest,installerInput:installer},runtimeImageReceiptSha256:hash('fixture image receipt'),providerReceiptSha256:hash('fixture provider receipt'),durabilityReceiptSha256:hash('fixture durability receipt'),freshVolumeReceiptSha256:hash('fixture fresh volume'),
    nativeSha256:contents[1][1].digest,launcherSha256:hash('fixture launcher'),serviceSha256:hash('fixture service'),nodeSha256:contents[2][1].digest,sidecarPolicySha256:hash('fixture sidecar'),volumeGb:1,cpus:1,memoryMb:512,userName:'Fixture',userEmail:'fixture@example.invalid'};
  return {image,policy:p,source,environment,all:[...all.values()]};
}
