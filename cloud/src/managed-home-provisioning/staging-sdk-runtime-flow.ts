import {inspect} from "node:util";
import type {WebSocket} from "ws";
import type {PostgresAuthorityConfig} from "../project-authority/config.js";
import type {ManagedHomePostgresConfig} from "./index.js";
import type {CompilerQualificationCandidate} from "./compiler-qualification-protocol.js";
import type {OriginalSdkRoleActivationPermit} from "./compiler-qualification.js";
import type {OriginalRuntimeRoleActivationPermit} from "./runtime-qualification.js";
import {CompilerSdkQualificationDriver} from "./compiler-qualification-driver.js";
import {RuntimeImageQualificationDriver} from "./runtime-qualification-driver.js";
import {StagingOrdinarySdkDriver} from "./staging-ordinary-sdk-driver.js";
import {assertStagingGenuineIngressExperiment,type StagingGenuineIngressExperiment} from "./staging-genuine-ingress-experiment.js";
import {createOriginalGenuineIngressCollector,type OriginalGenuineIngressCollector,type GenuineIngressExternalData} from "./staging-genuine-ingress-collector.js";
import {StagingRoleActivationAdmin,type StagingSetupRestriction} from "./staging-role-activation.js";
import {check,fail,snapshot,canonical,hash} from "./types.js";
const GENUINE_FLOW_EFFECTS_ALLOWED=false;
export type StagingGenuineIngressArtifactData=Omit<GenuineIngressExternalData,"app"|"machine"|"instance_id">;
export interface StagingGenuineIngressFlowInput {experiment:StagingGenuineIngressExperiment<WebSocket>;artifactData:StagingGenuineIngressArtifactData;output:string;}
const artifactKeys=["feature","image_digest","integrated_revision","source_manifest_sha256","root_receipt_manifest_sha256","fixture_manifest_sha256","native_elf_sha256","engine_elf_sha256"];
const exactFlowData=(v:unknown,keys:readonly string[]):v is Record<string,unknown>=>!!v&&typeof v==="object"&&!Array.isArray(v)&&Object.keys(v).length===keys.length&&keys.every(k=>Object.hasOwn(v,k));
function genuineArtifactData(v:unknown,experiment:StagingGenuineIngressExperiment<WebSocket>,image:string):StagingGenuineIngressArtifactData{
 check(exactFlowData(v,artifactKeys)&&artifactKeys.every(k=>typeof v[k]==="string"),"invalid_request");
 for(const k of ["source_manifest_sha256","root_receipt_manifest_sha256","fixture_manifest_sha256","native_elf_sha256","engine_elf_sha256"])check(/^[a-f0-9]{64}$/.test(v[k] as string)&&v[k]!=="0".repeat(64),"invalid_request");
 check(/^[a-f0-9]{40}$/.test(v.integrated_revision as string)&&v.integrated_revision!=="0".repeat(40)&&/^sha256:[a-f0-9]{64}$/.test(v.image_digest as string)&&v.image_digest!=="sha256:"+"0".repeat(64),"invalid_request");
 check(typeof image==="string"&&/^[^\s@]+@sha256:[a-f0-9]{64}$/.test(image)&&image.slice(image.lastIndexOf("@")+1)===v.image_digest,"identity_changed");
 check(v.root_receipt_manifest_sha256==="08ec75f53153899d855c53621a86d2781e89ce7bb12c77f97a55cd386dd0f264"&&v.fixture_manifest_sha256==="7a5a41afc55a7569a709d1d2224c3a5b1368ac16731f52f6b0f3411c6d7ecacf","invalid_request");
 const feature={CloseWins:"managed-ingress-fixture-close-wins",AdmissionWins:"managed-ingress-fixture-admission-wins",CarrierContention:"managed-ingress-fixture-carrier-contention"};
 check(v.feature===feature[experiment.statusData().case],"identity_changed");return snapshot(v) as StagingGenuineIngressArtifactData;
}
/** ONE original control/PG/policy lifetime. Each stage is a DISTINCT actual
 * provider request with its own existing 300/30/15 clocks. No stage retries,
 * saved-receipt activation or teardown/restart of original owners. */
export class StagingSdkRuntimeFlow {
 readonly #activation:StagingRoleActivationAdmin;#started=false;#promise?:Promise<void>;#stage="not-started";#failed=false;
 #sdk?:CompilerSdkQualificationDriver;#runtime?:RuntimeImageQualificationDriver;#ordinary?:StagingOrdinarySdkDriver;
 #sdkPermit?:OriginalSdkRoleActivationPermit;#runtimePermit?:OriginalRuntimeRoleActivationPermit;
 readonly #policyHash:string;readonly #genuine?:StagingGenuineIngressFlowInput;
 #genuineCommand?:Promise<void>;#genuineCollector?:OriginalGenuineIngressCollector;#genuineCapture?:Promise<void>;
 #genuineSettlement?:Promise<PromiseSettledResult<void>[]>;
 constructor(private readonly config:ManagedHomePostgresConfig,admin:PostgresAuthorityConfig,private readonly candidate:CompilerQualificationCandidate,
 private readonly account:string,private readonly commandKey:string,private readonly setup:StagingSetupRestriction,genuine?:StagingGenuineIngressFlowInput){
  if(genuine!==undefined){
   check(GENUINE_FLOW_EFFECTS_ALLOWED,"unconfigured"); // before optional input/config/admin access
   check(exactFlowData(genuine,["experiment","artifactData","output"]),"invalid_request");assertStagingGenuineIngressExperiment(genuine.experiment);
   check(typeof genuine.output==="string"&&genuine.output.startsWith("/")&&genuine.output.length<=4096&&!genuine.output.includes("\0"),"invalid_request");
   this.#genuine={experiment:genuine.experiment,artifactData:genuineArtifactData(genuine.artifactData,genuine.experiment,config.policy.image),output:genuine.output};
  }
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
  this.#stage="ordinary-provision";this.#ordinary=new StagingOrdinarySdkDriver(this.config,this.account,this.commandKey,this.setup,this.#genuine?.experiment);await this.#ordinary.startRetaining();
  if(this.#genuine){
   this.#current();const target=this.#ordinary.originalGenuineIngressTargetDataRetaining();
   const data:GenuineIngressExternalData={...this.#genuine.artifactData,...target};this.#stage="ordinary-genuine-ingress-historical-observation";
   // Retain the SAME original command before any fallible collector construction.
   // Observe rejection without replacing its promise; no second dispatch/retry.
   this.#genuineCommand=this.#ordinary.runGenuineIngressRetaining();void this.#genuineCommand.catch(()=>{});
   this.#genuineCollector=createOriginalGenuineIngressCollector(this.#ordinary.originalGenuineIngressObservationRetaining(),data,this.#genuine.output);
   this.#genuineCapture=this.#genuineCollector.startRetaining();void this.#genuineCapture.catch(()=>{});
   // Start BOTH originals before awaiting either. Readiness is preallocated on
   // the actual experiment; capture borrows only that original command budget.
   this.#genuineSettlement=Promise.allSettled([this.#genuineCommand,this.#genuineCapture]);
   const settled=await this.#genuineSettlement;check(settled.every(v=>v.status==="fulfilled"),"effect_unknown");
   this.#current();this.#stage="ordinary-genuine-ingress-settled-historical-data";
  }else{
   this.#stage="ordinary-sdk-cases";await this.#ordinary.runCasesRetaining();this.#current();this.#stage="ordinary-cases-settled-data";
  }
 }
 statusData(){return snapshot({format_version:1,scope:"one-original-staging-sdk-runtime-ordinary-flow-data-only",stage:this.#stage,failed:this.#failed,policy_sha256:this.#policyHash,
  sdk:this.#sdk?.statusData()??null,runtime:this.#runtime?.statusData()??null,activation:this.#activation.statusData(),ordinary:this.#ordinary?.statusData()??null,
  ...(this.#genuine?{genuine_ingress:{experiment:this.#genuine.experiment.statusData(),collector:this.#genuineCollector?.statusData()??null,command_retained:!!this.#genuineCommand,capture_retained:!!this.#genuineCapture,historical_data_only:true,qualified:false,current_claim:false}}:{}),
  owners_retained:true,production_enabled:false,source_acceptance_claim:false,cache_reuse_claim:false,shared_floor_retirement_claim:false},128*1024);}
}
