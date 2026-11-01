import {RootHostTransports} from "./transport.js";
import {migrateManagedHostTransports} from "./transport-schema.js";
import {RootOwnerRenewals} from "./renewals.js";
import {migrateManagedOwnerRenewals} from "./renewal-schema.js";
import {S3FabricObjectStorage} from "../fabric-cas.js";
import {ManagedFreshStorage,type StorageServiceCredential} from "./storage.js";
import {migrateManagedImageBatches} from "./storage-batch-schema.js";
import {migrateManagedStorage} from "./storage-schema.js";
import { AuthorityPool } from "../project-authority/pool.js";
import { budgets, validateConfig, type PostgresAuthorityConfig } from "../project-authority/config.js";
import { ManagedHomeRegistry } from "./registry.js";
import { ManagedHomePgStore } from "./store.js";
import { FlyHttpTransport,FlyManagedHomeProvider } from "./fly.js";
import { migrateManagedHomes } from "./schema.js";
import {RootEngineRegistration} from "./registration.js";
import {RootEngineOperations} from "./operations.js";
import {migrateManagedHomeOperations} from "./operations-schema.js";
import { check,fail,canonical,hash,validatePolicy,type ManagedHomePolicy } from "./types.js";

export interface ManagedStorageConfig {authority:PostgresAuthorityConfig;storage:S3FabricObjectStorage;storageReceiptSha256:string;serviceCredentials:readonly StorageServiceCredential[];}
export interface ManagedHomePostgresConfig {freshStorage?:ManagedStorageConfig;mode:"postgres";authority:PostgresAuthorityConfig;registrationAuthority?:PostgresAuthorityConfig;policy:ManagedHomePolicy;flyToken:string;tokenEncryptionKey:Buffer;}
export type ManagedHomeConfig={mode:"inactive"}|ManagedHomePostgresConfig;
function pool(c:PostgresAuthorityConfig,role:"runtime"|"migration"){try{validateConfig(c);}catch{fail("invalid_request");}const v=c.connection;
  return new AuthorityPool({host:v.host,port:v.port,database:v.database,user:v.user,password:v.password,ssl:{rejectUnauthorized:true,servername:v.host,...(v.ca?{ca:v.ca}:{})},application_name:"rudder-managed-home-provisioning"},budgets(c),role);}
/** No connection, environment scan, schema setup, provider mutation or legacy
 * adoption occurs at construction. Explicit settings come from trusted Cloud
 * operator custody; HTTP never accepts this configuration. */
export function createManagedHomeRegistry(config:ManagedHomeConfig={mode:"inactive"}):ManagedHomeRegistry{
  if(config.mode==="inactive"){check(Object.keys(config).length===1);return new ManagedHomeRegistry();}
  check(config.mode==="postgres"&&Object.keys(config).every(k=>["mode","authority","registrationAuthority","policy","flyToken","tokenEncryptionKey","freshStorage"].includes(k)));
  check(process.platform==="linux","unconfigured");
  const policy=validatePolicy(config.policy);check(Buffer.isBuffer(config.tokenEncryptionKey)&&config.tokenEncryptionKey.length===32&&config.tokenEncryptionKey.some(v=>v!==0));
  const provider=new FlyManagedHomeProvider(new FlyHttpTransport(config.flyToken));const db=pool(config.authority,"runtime");
  if(config.registrationAuthority){const a=config.authority.connection,r=config.registrationAuthority.connection;check(config.registrationAuthority.authorityId===config.authority.authorityId&&r.user!==a.user&&r.host===a.host&&r.port===a.port&&r.database===a.database&&r.ca===a.ca);}
  const store=new ManagedHomePgStore(config.authority.authorityId,db,hash(canonical(policy)),policy.ownerLeaseSafety?hash(canonical(policy.ownerLeaseSafety)):undefined);const registrationPool=config.registrationAuthority?pool(config.registrationAuthority,"runtime"):undefined;
  const registration=registrationPool?new RootEngineRegistration(registrationPool,config.authority.authorityId):undefined;
  let storage:ManagedFreshStorage|undefined;let storagePool:AuthorityPool|undefined;if(config.freshStorage){const f=config.freshStorage;check(config.registrationAuthority&&f.authority.authorityId===config.authority.authorityId&&f.storage instanceof S3FabricObjectStorage,"unconfigured");const a=config.authority.connection,c=f.authority.connection;check(a.host===c.host&&a.port===c.port&&a.database===c.database&&a.ca===c.ca&&a.user===c.user&&a.password===c.password,"unconfigured");storagePool=pool(f.authority,"runtime");storage=new ManagedFreshStorage({authorityId:f.authority.authorityId,pool:storagePool,storage:f.storage,storageReceiptSha256:f.storageReceiptSha256,serviceCredentials:f.serviceCredentials});}
  const operations=registrationPool?new RootEngineOperations(registrationPool,config.authority.authorityId,store,storage,storage?new RootOwnerRenewals(storagePool!,config.authority.authorityId,storage):undefined):undefined;
  return new ManagedHomeRegistry(store,provider,policy,config.tokenEncryptionKey,registration,operations,storage,registrationPool?new RootHostTransports(registrationPool,config.authority.authorityId):undefined);
}
/** Explicit admin-only frozen schema installation, after existing project
 * authority migration. No activation method and no runtime-role DDL. */
export function createManagedHomeMigrator(config:PostgresAuthorityConfig,policyInput:ManagedHomePolicy){const policy=validatePolicy(policyInput);const db=pool(config,"migration");return {
  migrate:(runtimeRole:string,registrationRole?:string)=>migrateManagedHomes(db,config.authorityId,hash(canonical(policy)),runtimeRole,registrationRole),close:()=>db.close(),};}
export function createManagedHomeOperationMigrator(config:PostgresAuthorityConfig){const db=pool(config,"migration");return {migrate:(runtimeRole:string,registrationRole:string)=>migrateManagedHomeOperations(db,runtimeRole,registrationRole),close:()=>db.close()};}
export {ManagedHomeError} from "./types.js";
export type {ManagedHomeRegistry,ManagedHomeRoute,ManagedHomeWorker,FreshManagedProjectGrant,RegisteredEngineAdmission,ManagedCommandSend} from "./registry.js";
export type {ManagedHomePolicy,HomeSpec,PreparedProject,HostOperation,ProvisionReceipt,ProvisionPhase,RuntimeManifest} from "./types.js";
export type {Envelope,RegistrationBegin,RegistrationChallenge,RegistrationProof,RegistrationAck,RegistrationBinding,RegistrationEvidence} from "./registration.js";
export type {EngineOperation,EngineOperationRequest,EngineOperationAck,EngineOperationError} from "./operations.js";

export function createManagedStorageMigrator(config:PostgresAuthorityConfig,storageReceiptSha256:string){const db=pool(config,"migration");return {migrate:(roles:readonly string[])=>migrateManagedStorage(db,config.authorityId,storageReceiptSha256,roles),close:()=>db.close()};}
export type {StorageOperation} from "./storage-protocol.js";
export type {StorageServiceCredential} from "./storage.js";

export function createManagedOwnerRenewalMigrator(config:PostgresAuthorityConfig,clockPolicySha256:string){const db=pool(config,"migration");return {migrate:(runtimeRole:string)=>migrateManagedOwnerRenewals(db,config.authorityId,clockPolicySha256,runtimeRole),close:()=>db.close()};}
export type {OwnerLeaseSafety} from "./types.js";

/** Explicit frozen operator migration; installation alone cannot authenticate a socket. */
export function createManagedHostTransportMigrator(config:PostgresAuthorityConfig){const db=pool(config,"migration");return {migrate:(registrationRole:string)=>migrateManagedHostTransports(db,config.authorityId,registrationRole),close:()=>db.close()};}
export type {CommittedHostTransport} from "./transport.js";

/** Explicit operator-only FROZEN install, no default migration or enablement. */
export function createManagedImageBatchMigrator(config:PostgresAuthorityConfig,qualificationReceiptSha256:string){const db=pool(config,"migration");return {migrate:(runtimeRoles:readonly string[])=>migrateManagedImageBatches(db,config.authorityId,qualificationReceiptSha256,runtimeRoles),close:()=>db.close()};}
