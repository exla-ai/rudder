import test from "node:test";
import assert from "node:assert/strict";
import {randomUUID} from "node:crypto";
import {readManagedHomeStartupConfig} from "../cloud/dist/managed-home-provisioning/startup-config.js";
import {createManagedHomeRegistry} from "../cloud/dist/managed-home-provisioning/index.js";
import {S3FabricObjectStorage,canonicalFabricMetadata} from "../cloud/dist/fabric-cas.js";
import {componentGraph,memoryS3} from "./managed-storage-pg-fixtures.mjs";

function fixture(){
  const authority={mode:"postgres",authorityId:randomUUID(),connection:{host:"db.fixture.invalid",port:5432,database:"rudder",user:"runtime",password:"PRIVATE_FIXTURE_PASSWORD"}};
  const value={formatVersion:1,authority,registrationAuthority:{...authority,connection:{...authority.connection,user:"registration",password:"PRIVATE_REGISTRATION_PASSWORD"}},
    policy:componentGraph(canonicalFabricMetadata).policy,tokenEncryptionKeyHex:"a".repeat(64),storageReceiptSha256:"b".repeat(64),
    serviceCredentials:[{accountId:"fixture-account",principalId:"storage-service",credentialId:"storage-service",tokenBase64url:Buffer.alloc(32,7).toString("base64url")}]};
  const dependencies={storage:memoryS3(S3FabricObjectStorage).storage,flyToken:"PRIVATE_FIXTURE_PROVIDER_TOKEN"};
  return {value,dependencies};
}
function refuses(value,dependencies){
  assert.throws(()=>readManagedHomeStartupConfig(value,dependencies),error=>{
    assert.equal(error.message,"Invalid private managed-home startup configuration");
    assert.equal(error.cause,undefined);
    assert(!String(error).includes("PRIVATE_"));
    return true;
  });
}

test("absent operator configuration stays inactive without storage or provider credentials",()=>{
  assert.deepEqual(readManagedHomeStartupConfig(undefined,{storage:null,flyToken:""}),{mode:"inactive"});
});
test("private startup wiring preserves distinct registration and the same runtime storage login",async()=>{
  const {value,dependencies}=fixture();const config=readManagedHomeStartupConfig(JSON.stringify(value),dependencies);
  assert.equal(config.mode,"postgres");assert.equal(config.freshStorage.authority,config.authority);
  assert.notEqual(config.registrationAuthority.connection.user,config.authority.connection.user);
  assert.equal(config.freshStorage.storage,dependencies.storage);
  assert.deepEqual(config.tokenEncryptionKey,Buffer.from(value.tokenEncryptionKeyHex,"hex"));
  assert.deepEqual(config.freshStorage.serviceCredentials[0].token,Buffer.alloc(32,7));
  // Lazy construction/close uses the actual production factory. There is no
  // migration, ready-row change, provider call or real database connection.
  if(process.platform==="linux"){
    const registry=createManagedHomeRegistry(config);await registry.close();
  }else{
    assert.throws(()=>createManagedHomeRegistry(config),error=>error.code==="unconfigured");
  }
});
test("explicit malformed configuration never silently falls back or exposes private input",()=>{
  const {value,dependencies}=fixture();
  for(const raw of ["","PRIVATE_FIXTURE_TOKEN",JSON.stringify({...value,ready:true}),"x".repeat(256*1024+1)])refuses(raw,dependencies);
});
test("configuration cannot supply another storage backend or omit trusted process dependencies",()=>{
  const {value,dependencies}=fixture();const raw=JSON.stringify(value);
  refuses(raw,{...dependencies,storage:null});refuses(raw,{...dependencies,storage:{}});refuses(raw,{...dependencies,flyToken:""});
});
test("private credentials require exact roles, identities and canonical nonzero keys",()=>{
  const {value,dependencies}=fixture();
  const change=edit=>{const copy=structuredClone(value);edit(copy);refuses(JSON.stringify(copy),dependencies);};
  change(c=>{c.registrationAuthority.connection.user=c.authority.connection.user;});
  change(c=>{c.registrationAuthority.connection.host="another.fixture.invalid";});
  change(c=>{c.tokenEncryptionKeyHex="0".repeat(64);});
  change(c=>{c.serviceCredentials[0].tokenBase64url+="=";});
  change(c=>{c.serviceCredentials[0].tokenBase64url=Buffer.alloc(32).toString("base64url");});
  change(c=>{c.serviceCredentials.push({...c.serviceCredentials[0]});});
  change(c=>{c.authority.connection.rejectUnauthorized=false;});
});
