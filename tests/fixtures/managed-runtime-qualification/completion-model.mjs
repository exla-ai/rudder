// Explicit model PG/Fly/Root evidence; production HMAC/transport/source-owner
// mechanisms are called. Never physical Root, real PG, provider or enable proof.
import assert from 'node:assert/strict';
import http from 'node:http';
import {once} from 'node:events';
import {createRequire} from 'node:module';
const require=createRequire(new URL('../../../cloud/package.json',import.meta.url)),{WebSocket,WebSocketServer}=require('ws');
import {createHmac,randomUUID} from 'node:crypto';
import {flyFetchFixture} from '../../managed-storage-pg-fixtures.mjs';
import {policy,candidate,scope,ready} from './data.mjs';
import {FlyHttpTransport,FlyManagedHomeProvider,originalTime} from '../../../cloud/dist/managed-home-provisioning/fly.js';
import {RootHostTransports,transportMac} from '../../../cloud/dist/managed-home-provisioning/transport.js';
import {ManagedHomeRegistry} from '../../../cloud/dist/managed-home-provisioning/registry.js';
import {RootRuntimeImageQualifications} from '../../../cloud/dist/managed-home-provisioning/runtime-qualification.js';
import {managedHomeProtectedConfigData} from '../../../cloud/dist/managed-home-provisioning/protected-config.js';
import {reserveFreshProjects,freshSpec} from '../../../cloud/dist/managed-home-provisioning/fresh.js';
import {binding} from '../../../cloud/dist/managed-home-provisioning/registration.js';
import {canonical,hash,EFFECTS} from '../../../cloud/dist/managed-home-provisioning/types.js';
import {runtimeQualificationMac} from '../../../cloud/dist/managed-home-provisioning/runtime-qualification-protocol.js';
import {TRANSPORT_MIGRATION_SHA256} from '../../../cloud/dist/managed-home-provisioning/transport-schema.js';
import {RUNTIME_QUALIFICATION_MIGRATION_SHA256} from '../../../cloud/dist/managed-home-provisioning/runtime-qualification-schema.js';
import {RUNTIME_ROLE_COMPLETION_MIGRATION_SHA256} from '../../../cloud/dist/managed-home-provisioning/runtime-qualification-completion-schema.js';

export class ModelPool {
 constructor(p){this.policy=p;this.q=null;this.role=null;this.completion=null;this.transports=new Map();this.hook=null;this.afterCommit=null;this.events=[];this.auth=null;}
 async run(account,work){const saved=structuredClone({q:this.q,role:this.role,completion:this.completion,transports:this.transports});let phase='read';
  const query=async(sql,a=[])=>{this.events.push(sql);if(this.hook)await this.hook(sql,a);
   if(sql.startsWith('SELECT pg_advisory')||sql.startsWith('DELETE FROM'))return {rows:[],rowCount:0};
   if(sql.includes('SELECT i.state,i.policy_sha256'))return {rows:[{state:'ready',authority_state:'ready',home_state:'ready',policy_sha256:hash(canonical(this.policy)),home_policy:hash(canonical(this.policy)),migration_sha256:RUNTIME_QUALIFICATION_MIGRATION_SHA256,version_sha256:RUNTIME_QUALIFICATION_MIGRATION_SHA256}]};
   if(sql.includes('SELECT sha256')&&sql.includes('managed_runtime_role_completion_migrations'))return {rows:[{sha256:RUNTIME_ROLE_COMPLETION_MIGRATION_SHA256}]};
   if(sql.includes('SELECT state,migration_sha256')&&sql.includes('managed_host_transport_installation'))return {rows:[{state:'ready',migration_sha256:TRANSPORT_MIGRATION_SHA256}]};
   if(sql.includes('SELECT r.ack_json,r.challenge_json'))return {rows:[{ack_json:this.auth.actual.ack,challenge_json:{body:this.auth.actual.ack.body},token_sha256:this.auth.tokenHash}]};
   if(sql.includes('SELECT r.ack_json,c.token_sha256')){const row=this.transports.get(a[3]);return {rows:row?.state==='committed'&&row.generation===a[4]?[{ack_json:this.auth.actual.ack,token_sha256:this.auth.tokenHash}]:[]};}
   if(sql.startsWith('SELECT serial::text')){const row=[...this.transports.values()].sort((x,y)=>BigInt(x.serial)>BigInt(y.serial)?-1:1)[0];return {rows:row?[{...row,live:true}]:[]};}
   if(sql.includes('SELECT count(*)')&&sql.includes('managed_host_transports'))return {rows:[{n:String([...this.transports.values()].filter(x=>x.state==='pending').length)}]};
   if(sql.startsWith('INSERT INTO')&&sql.includes('managed_host_transports')){this.transports.set(a[5],{serial:a[5],generation:a[6],state:'pending'});return {rows:[],rowCount:1};}
   if(sql.includes("SET state='superseded'")){for(const row of this.transports.values())if(row.state==='committed')row.state='superseded';return {rows:[],rowCount:1};}
   if(sql.includes("SET state='committed'")){this.transports.get(a[3]).state='committed';return {rows:[],rowCount:1};}
   if(sql.includes('SELECT 1 FROM')&&sql.includes('managed_host_transports')){const row=this.transports.get(a[3]);return {rows:row?.state==='committed'&&row.generation===a[4]?[{}]:[]};}
   if(sql.includes('SELECT count(*)')&&sql.includes('managed_runtime_qualifications'))return {rows:[{n:this.q?'1':'0'}]};
   if(sql.startsWith('INSERT INTO')&&sql.includes('managed_runtime_qualifications')){this.q={authority_id:a[0],account_id:a[1],qualification_id:a[2],home_id:a[3],process_owner:a[4],provision_nonce:a[5],request_sha256:a[6],scope_json:JSON.parse(a[7]),state:'reserved',live:true};return {rows:[],rowCount:1};}
   if(sql.includes("SET state='started'")){assert.equal(this.q.state,'reserved');Object.assign(this.q,{state:'started',ready_request_sha256:a[3],ready_request_json:JSON.parse(a[4]),registration_proof_sha256:a[5]});return {rows:[{scope_json:this.q.scope_json}],rowCount:1};}
   if(sql.startsWith('SELECT *')&&sql.includes('managed_runtime_qualifications'))return {rows:this.q?[structuredClone(this.q)]:[]};
   if(sql.includes('SELECT r.ack_json,r.proof_json')){const {actual,home}=this.auth,ack=actual.ack.body;return {rows:[{ack_json:actual.ack,proof_json:{evidence:{service:{pid:103}}},intent_json:home.intent,nonce:home.intent.nonce,revoked_at:null,engine_revoked:null,credential_revoked:null,credential_live:true,authority_state:'ready',home_state:'ready',engine_instance_id:ack.engine_instance_id,machine_id:ack.binding.machine_id,machine_installation_id:ack.binding.incarnation,machine_boot_id:ack.boot_id,authenticated_key_id:ack.credential_id,token_sha256:actual.context.credentialSha256,principal_id:actual.context.principalId}]};}
   if(sql.startsWith('INSERT INTO')&&sql.includes('managed_runtime_image_roles')){phase='prepared';this.role??={authority_id:a[0],role_binding_sha256:a[1],image:a[2],image_receipt_sha256:a[3],inventory_sha256:a[4],binding_json:JSON.parse(a[5]),qualification_id:a[6],ready_request_sha256:a[7]};return {rows:[],rowCount:1};}
   if(sql.startsWith('SELECT *')&&sql.includes('managed_runtime_image_roles'))return {rows:this.role?[structuredClone(this.role)]:[]};
   if(sql.includes("SET state='published'")){phase='prepared';Object.assign(this.q,{state:'published',ack_json:JSON.parse(a[3])});return {rows:[],rowCount:1};}
   if(sql.startsWith('INSERT INTO')&&sql.includes('managed_runtime_role_completions')){phase='completion';this.completion??={authority_id:a[0],role_binding_sha256:a[1],qualification_id:a[2],ready_request_sha256:a[3],completion_sha256:a[4],completion_json:JSON.parse(a[5])};return {rows:[],rowCount:1};}
   if(sql.startsWith('SELECT completion_sha256'))return {rows:this.completion?[structuredClone(this.completion)]:[]};
   if(sql.startsWith('SELECT r.*,c.completion'))return {rows:this.role&&this.completion?[{...structuredClone(this.role),completion_sha256:this.completion.completion_sha256,completion_json:structuredClone(this.completion.completion_json)}]:[]};
   if(sql.startsWith('SELECT q.state'))return {rows:this.q?[{state:this.q.state,ready_request_sha256:this.q.ready_request_sha256,ack_json:this.q.ack_json??null,completion_json:this.completion?.completion_json??null}]:[]};
   throw Error('unsupported model SQL');
  };
  let value;try{value=await work({query});}catch(error){Object.assign(this,saved);throw error;}
  // Deliberately AFTER modeled COMMIT: no rollback of persisted DATA. The hook
  // can close the actual socket or lose the original COMMIT response.
  const outcome=this.afterCommit?await this.afterCommit(phase):null;
  return outcome==='unknown'?{status:'unknown'}:{status:'committed',value};
 }
}

export async function fixture(){
 let previous=globalThis.fetch,server,wss,client;
 try {
 const p=policy(),pool=new ModelPool(p),authorityId=randomUUID(),i={authorityId,accountId:'fixture-account',homeId:'mh-'+randomUUID(),commandKey:'qualification',requestHash:'a'.repeat(64),epoch:'1',nonce:randomUUID(),appName:'new-'+randomUUID(),networkName:'new-network-'+randomUUID(),storeId:randomUUID(),incarnation:randomUUID(),processOwner:randomUUID(),policy:p};
 i.runtimeQualification=scope(p);i.freshProjects=reserveFreshProjects(['Inert fixture']);i.spec=freshSpec(i.freshProjects,i.storeId,p);
 const source=new RootRuntimeImageQualifications(pool,pool,authorityId,p),purpose=await source.reserveRetaining(i,originalTime(300000));
 const fake=flyFetchFixture();previous=globalThis.fetch;globalThis.fetch=fake.fetch;
 const provider=new FlyManagedHomeProvider(new FlyHttpTransport('synthetic-managed-pg-provider-only',128));const acks={};
 try{for(const e of EFFECTS)acks[e]=await provider.execute(e,i,acks,e.endsWith('_secret')?'synthetic private fixture value':undefined,originalTime(30000));}catch(e){globalThis.fetch=previous;throw e;}
 const home={intent:i,phase:'routable',tokenHash:'f'.repeat(64),acks},custody=provider.captureOriginalDisposableMachine(home),b=binding(home,hash(managedHomeProtectedConfigData(i,acks.machine))),r=ready(p);
 r.scope=i.runtimeQualification;r.qualification_id=r.scope.qualification_id;r.qualification_nonce=r.scope.qualification_nonce;r.registration_binding=b;
 const rootKey=Buffer.alloc(32,11),body={version:1,kind:'ack',challenge_id:randomUUID(),challenge_nonce:'d'.repeat(64),root_nonce:r.root_nonce,engine_instance_id:r.observation.engine_instance_id,credential_id:randomUUID(),binding:b,constraint:r.observation.original_enable_constraint,boot_id:r.observation.boot_id,proof_sha256:r.registration_proof_sha256};
 r.observation.config_sha256=b.config_sha256;r.observation.epoch=b.epoch;
 const credential=Buffer.from('rdre_'+createHmac('sha256',rootKey).update('rudder_engine_registration_credential_v1\0').update(canonical(Object.fromEntries(['challenge_id','engine_instance_id','credential_id','root_nonce'].map(k=>[k,body[k]])))).digest('hex'));
 const actual={ack:{body,mac:'e'.repeat(64)},context:{accountId:i.accountId,principalId:'registered-engine:'+body.engine_instance_id,credentialId:body.credential_id,credentialSha256:hash(credential)},proofSha256:body.proof_sha256};pool.auth={actual,home,tokenHash:hash(credential)};
 server=http.createServer();wss=new WebSocketServer({server});await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));
 const connected=once(wss,'connection');client=new WebSocket('ws://127.0.0.1:'+server.address().port);const opened=once(client,'open');
 const [socket]=await connected;await opened;
 const transports=new RootHostTransports(pool,authorityId),start={version:1,kind:'host_transport_begin',registration:body,serial:'1',generation:randomUUID(),attempt_nonce:randomUUID(),constraint:{format_version:1,maximum_milliseconds:5000,boottime_deadline_nanoseconds:'9999999999999999999'},remaining_milliseconds:5000};
 const challenge=await transports.begin(home,rootKey,{body:start,mac:transportMac(credential,'begin',start)},socket,originalTime(5000));const commit={...challenge.body,kind:'host_transport_commit',native_acceptance_sha256:'a'.repeat(64)};
 const active=await transports.commit(home,rootKey,{body:commit,mac:transportMac(credential,'commit',commit)},socket);
 const registry=new ManagedHomeRegistry(undefined,undefined,undefined,undefined,undefined,undefined,undefined,transports);
 socket.on('close',()=>registry.closeManagedHostSocket(socket));const original=originalTime(30000);
 return {p,pool,source,purpose,home,custody,actual,transport:active.owner,registry,socket,original,
  publish:()=>source.publishRetaining(purpose,custody,home,actual,active.owner,rootKey,{body:r,mac:runtimeQualificationMac(rootKey,'ready',r)},original),
  close:()=>registry.closeManagedHostSocket(socket),restore:async()=>{globalThis.fetch=previous;client.terminate();socket.terminate();await new Promise(resolve=>wss.close(resolve));await new Promise(resolve=>server.close(resolve));}};
 }catch(error){globalThis.fetch=previous;client?.terminate();if(wss){for(const socket of wss.clients)socket.terminate();await new Promise(resolve=>wss.close(resolve));}if(server?.listening)await new Promise(resolve=>server.close(resolve));throw error;}
}
