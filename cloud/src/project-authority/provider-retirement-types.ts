import type {ProviderOwnerFence,ProviderBinding} from "./types.js";
/** Intent only. No body field can claim physical cessation/reconciliation. */
export interface ProviderRetirementRequest extends ProviderOwnerFence {projectId:string;idempotencyKey:string;action:"release"|"replace";}
export interface ProviderRetirementData {formatVersion:1;scope:"provider-owner-admission-closed-only";authorityId:string;accountId:string;projectId:string;
 retirementId:string;action:"release"|"replace";ownerEpoch:string;ownershipId:string;engineInstanceId:string;machineBootId:string;
 provider:ProviderBinding;previousProjectMode:string;inventory:ProviderRetirementInventory;physicalFenceObserved:false;effectsReconciled:false;ownerTransferred:false;}
export interface ProviderRetirementInventory {formatVersion:1;scope:"bounded-historical-reconciliation-inventory-only";effectCount:string;outboxCount:string;
 effects:Record<string,unknown>[];outbox:Record<string,unknown>[];overflow:boolean;requiresNativeJournalReconciliation:true;complete:false;}
export interface ProviderFenceDispatchData {formatVersion:1;scope:"original-provider-fence-request-only";retirement:ProviderRetirementData;fenceAttemptId:string;physicalFenceObserved:false;}
