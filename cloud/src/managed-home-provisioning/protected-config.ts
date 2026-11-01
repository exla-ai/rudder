import {validateCompilerQualificationScope} from "./compiler-qualification-protocol.js";
import {validateRuntimeQualificationScope} from "./runtime-qualification-protocol.js";
import {canonical,controlOrigin,runtimeMountConfigData,
  type Intent,type MachineAck} from "./types.js";
/** The ONE existing canonical protected-config producer, extracted without a
 * writer/provider effect. Buffer DATA grants no process/Root/runtime authority.
 * The actual registry uses it for secret bytes AND both registration hashes.
 * A future scoped disposable qualifier must reuse it, not insert ad-hoc JSON.
 * Production selected-role admission is independently refused until a genuine
 * inventory+role qualification publisher/record/consumer exists. */
export function managedHomeProtectedConfigData(i:Intent,machine:MachineAck):Buffer {
  const runtime=runtimeMountConfigData(i.policy);
  const qualification=i.runtimeQualification===undefined?undefined:validateRuntimeQualificationScope(i.runtimeQualification,i.policy);
  return canonical({version:1,machine_id:machine.id,store_id:i.storeId,incarnation:i.incarnation,
    account_id:i.accountId,home_id:i.homeId,cloud_origin:controlOrigin(i.policy),
    state_directory:i.spec.stateDirectory,store_policy:i.spec.storePolicy,projects:i.spec.projects,
    runtime_profile:{FlyManagedImageV1:{user_name:i.spec.userName,user_email:i.spec.userEmail}},
    runtime_manifest:i.policy.runtimeManifest,
    ...(qualification?{runtime_qualification:qualification}:{}),
    ...(i.policy.compilerSdk?{compiler_sdk:i.policy.compilerSdk}:{}),
    ...(!i.runtimeQualification&&!i.compilerQualification&&i.policy.sdkInitialSource?{sdk_initial_source:i.policy.sdkInitialSource}:{}),
    ...(i.compilerQualification?{compiler_qualification:validateCompilerQualificationScope(i.compilerQualification,i.policy)}:{}),
    ...(i.policy.ownerMode?{owner_mode:i.policy.ownerMode}:{}),
    ...(i.policy.ownerLeaseSafety?{owner_lease_safety:{
      qualification_receipt_sha256:i.policy.ownerLeaseSafety.qualificationReceiptSha256,
      margin_milliseconds:i.policy.ownerLeaseSafety.marginMilliseconds,
      engine_rate_error_ppm:i.policy.ownerLeaseSafety.engineRateErrorPpm,
      database_rate_error_ppm:i.policy.ownerLeaseSafety.databaseRateErrorPpm,
      max_positive_clock_step_milliseconds:i.policy.ownerLeaseSafety.maxPositiveClockStepMilliseconds}}:{}),
    ...(i.policy.imagePreseed?{image_preseed:{image:i.policy.image,
      image_receipt_sha256:i.policy.runtimeImageReceiptSha256,
      full_manifest_sha256:i.policy.imagePreseed.fullManifestSha256,
      ...(i.policy.imagePreseed.batchTransport?{batch_transport:i.policy.imagePreseed.batchTransport}:{}),
      ...(runtime?{runtime_mount:runtime}:{})}}:{})},32*1024);
}
