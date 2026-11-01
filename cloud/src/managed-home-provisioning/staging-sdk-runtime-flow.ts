import {inspect} from "node:util";
import type {PostgresAuthorityConfig} from "../project-authority/config.js";
import type {ManagedHomePostgresConfig} from "./index.js";
import type {CompilerQualificationCandidate} from "./compiler-qualification-protocol.js";
import type {OriginalSdkRoleActivationPermit} from "./compiler-qualification.js";
import type {OriginalRuntimeRoleActivationPermit} from "./runtime-qualification.js";
import {CompilerSdkQualificationDriver} from "./compiler-qualification-driver.js";
import {RuntimeImageQualificationDriver} from "./runtime-qualification-driver.js";
import {StagingOrdinarySdkDriver} from "./staging-ordinary-sdk-driver.js";
import {StagingRoleActivationAdmin,type StagingSetupRestriction} from "./staging-role-activation.js";
import {check,fail,snapshot,canonical,hash} from "./types.js";
/** ONE original control/PG/policy lifetime. Each stage is a DISTINCT actual
 * provider request with its own existing 300/30/15 clocks. No stage retries,
 * saved-receipt activation or teardown/restart of original owners. */
export class StagingSdkRuntimeFlow {
 readonly #activation:StagingRoleActivationAdmin;#started=false;#promise?:Promise<void>;#stage="not-started";#failed=false;
 #sdk?:CompilerSdkQualificationDriver;#runtime?:RuntimeImageQualificationDriver;#ordinary?:StagingOrdinarySdkDriver;
 #sdkPermit?:OriginalSdkRoleActivationPermit;#runtimePermit?:OriginalRuntimeRoleActivationPermit;
 readonly #policyHash:string;
 constructor(private readonly config:ManagedHomePostgresConfig,admin:PostgresAuthorityConfig,private readonly candidate:CompilerQualificationCandidate,
 private readonly account:string,private readonly commandKey:string,private readonly setup:StagingSetupRestriction){
  check(config.policy.controlPlane?.profile==="qualification-v1"&&config.policy.compilerSdk&&config.policy.sdkInitialSource==="sdk-fixed-suite-v1"&&config.policy.ownerMode==="provider-fenced-v1"&&!config.policy.ownerLeaseSafety&&config.freshStorage,"unconfigured");
  check(commandKey.length<=96,"invalid_request");this.#policyHash=hash(canonical(config.policy));this.#activation=new StagingRoleActivationAdmin(admin,config.authority,config.policy,setup);
 }
 [inspect.custom](){return "StagingSdkRuntimeFlow [all original campaigns/admin/ordinary owners retained]";}
 toJSON():never{fail("invalid_request");}
 #current(){this.setup.remaining();check(hash(canonical(this.config.policy))===this.#policyHash,"identity_changed");}
 async #wait(){this.#current();await new Promise<void>(resolve=>setTimeout(resolve,200));this.#current();}
 runRetaining():Promise<void>{check(!this.#started,"effect_unknown");this.#started=true;
  this.#promise=this.#run().catch(()=>{this.#failed=true;this.#stage="quarantined-retained";fail("effect_unknown");});return this.#promise;
 }
 async #run():Promise<void>{
  this.#current();const {freshStorage,...qualifierConfig}=this.config;void freshStorage;
  this.#stage="sdk-qualification";this.#sdk=new CompilerSdkQualificationDriver(qualifierConfig,this.candidate,this.account,this.commandKey+":sdk");await this.#sdk.startRetaining();
  while(!this.#sdkPermit){this.#current();this.#sdkPermit=this.#sdk.takeOriginalRoleActivationPermit();if(!this.#sdkPermit)await this.#wait();}
  this.#stage="sdk-admin-activation";await this.#activation.activateRetaining(this.#sdkPermit);this.#current();
  this.#stage="sdk-new-listener-restricted";await this.#sdk.closeNewListenerAdmissionRetaining();this.#current();
  const {sdk,...runtimeCandidate}=this.candidate;void sdk;
  this.#stage="runtime-qualification";this.#runtime=new RuntimeImageQualificationDriver(qualifierConfig,runtimeCandidate,this.account,this.commandKey+":runtime");await this.#runtime.startRetaining();
  while(!this.#runtimePermit){this.#current();this.#runtimePermit=this.#runtime.takeOriginalRoleActivationPermit();if(!this.#runtimePermit)await this.#wait();}
  this.#stage="runtime-admin-activation";await this.#activation.activateRetaining(this.#runtimePermit);this.#current();
  this.#stage="runtime-new-listener-restricted";await this.#runtime.closeNewListenerAdmissionRetaining();this.#current();
  this.#stage="ordinary-provision";this.#ordinary=new StagingOrdinarySdkDriver(this.config,this.account,this.commandKey,this.setup);await this.#ordinary.startRetaining();
  this.#stage="ordinary-sdk-cases";await this.#ordinary.runCasesRetaining();this.#current();this.#stage="ordinary-cases-settled-data";
 }
 statusData(){return snapshot({format_version:1,scope:"one-original-staging-sdk-runtime-ordinary-flow-data-only",stage:this.#stage,failed:this.#failed,policy_sha256:this.#policyHash,
  sdk:this.#sdk?.statusData()??null,runtime:this.#runtime?.statusData()??null,activation:this.#activation.statusData(),ordinary:this.#ordinary?.statusData()??null,
  owners_retained:true,production_enabled:false,source_acceptance_claim:false,cache_reuse_claim:false,shared_floor_retirement_claim:false},128*1024);}
}
