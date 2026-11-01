import {randomUUID} from "node:crypto";
import {check,hash,validateSpec,type FreshProjectReservation,type ManagedHomePolicy,type HomeSpec} from "./types.js";
/** NEW empty logical projects only. No repository URL, existing UUID, checkout,
 * imported source, saved-ready flag or legacy owner enters this allocator. */
export function reserveFreshProjects(names:readonly string[]):FreshProjectReservation[]{check(Array.isArray(names)&&names.length>0&&names.length<=8);const seen=new Set<string>();return names.map(title=>{
  check(typeof title==="string"&&/^[A-Za-z][A-Za-z0-9 _-]{0,79}$/.test(title)&&!/^[a-f0-9]{8}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{12}$/.test(title)&&title.trim()===title&&!seen.has(title));seen.add(title);const projectId=randomUUID();return {projectId,workspaceId:"mp-"+randomUUID(),prepareId:randomUUID(),title,projectKey:"fresh-v4:"+projectId};});}
export function freshSpec(projects:FreshProjectReservation[],storeId:string,policy:ManagedHomePolicy):HomeSpec{return validateSpec({
  stateDirectory:"/workspace/managed-control/"+storeId,storePolicy:"InitializeEmptyV1",userName:policy.userName,userEmail:policy.userEmail,
  projects:projects.map(p=>{const base="/workspace/fresh-v4/"+p.projectId;const home=base+"/home";const checkout=base+"/repository";return {
    alias:p.workspaceId,checkout,home,cas:"/workspace/managed-control/"+storeId+"/cas",endpoints:base+"/endpoints",prepare:{request_id:p.prepareId,namespace:{format_version:1 as const,project_id:p.projectId,home,journal:home+"/fabric/journal.sqlite",repository:checkout,directory:home+"/fabric/managed-projects/"+hash(p.projectId)}}};}),
});}
/** Immutable DATA copied from the authoritative committed assignment; only the
 * registry's private capability issuer may wrap it after the actual PG result. */
export interface FreshAssignment {accountId:string;homeId:string;workspaceId:string;projectId:string;commandKey:string;nonce:string;engineInstance:string;bootId:string;ownerEpoch:string;credentialId:string;ownerReceipt:unknown;}
