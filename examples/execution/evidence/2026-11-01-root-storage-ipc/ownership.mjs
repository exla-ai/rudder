// Private stock-image DATA and exact Machine ownership. Import has no effects.
import {randomUUID, createHash} from 'node:crypto';
import {validateQualifierOutput} from './qualifier-marker.mjs';
export const APP = 'rudder-workers-exla';
export const ROLE = 'root-storage-ipc-kernel-canary';
export const IMAGE = 'registry.fly.io/rudder-workers-exla:root-storage-ipc-canary';
// Modified working-source base revision, not a release claim. Package/base image remain alpha23.
export const REVISION = '4885ef6fdeb1992420bdda975dc9df6c198c421b';
export const VERSION = '4.0.0-alpha.23';
export const BASE_IMAGE = 'registry.fly.io/rudder-workers-exla:4.0.0-alpha.23';
export const BASE_IMAGE_DIGEST = 'sha256:ea39437f476ea1c704afb7eca6596d79df9dfada97ae9dde74141562e7394f80';
// FAIL-CLOSED SENTINELS. Only Root's actual frozen source/ELF/image build
// receipts may replace these. This working-tree qualifier is not a release.
export const DERIVED_IMAGE_DIGEST = 'sha256:af9b8877bfb2102b69d6a090a3dae565e27ba0719f68ffa1f4da4ecc935ec86c';
export const QUALIFIER_ELF_SHA256 = 'fb5e307cf1833fd59e0e70239fd1cbbe516f0ef5c7a6254726d23f4a924542b1';
export const SOURCE_TREE_SHA256 = '5c48947ca489350374d5acde1ceb3bc36be0df090f56b18dfe53b6fecf56a509';
export const QUALIFIER_PATH = '/usr/local/bin/rudder-root-seed-test';
export const QUALIFIER_TEST = 'root_storage_service::ipc_qualification_tests::root_storage_ipc_kernel_qualification';
export const QUALIFIER_ARGS = Object.freeze(['--exact',QUALIFIER_TEST,'--ignored','--nocapture','--test-threads=1']);
export const FORBIDDEN_ID = '8edd225a7d0378';
export const FORBIDDEN_INSTANCE = '01M670MG4A8E0DWE9GA1C0PQ92';
export const FORBIDDEN_IMAGE_DIGEST = 'sha256:404f46396aca77324d9b53c55d2824a5401af56ba5362d0bdae67e16108fc245';
export const ROOT_FLAGS = Object.freeze({PATH:'/usr/local/bin:/usr/bin:/bin',
  RUDDER_DISABLE_UPDATE_CHECK:'1', RUDDER_DISABLE_AUTO_UPDATE:'1', RUDDER_SKIP_AUTO_UPDATE:'1',
  RUDDER_DISABLE_ONBOARD_INSTALL:'1', RUDDER_TELEMETRY:'off'});
export const sha = bytes => createHash('sha256').update(bytes).digest('hex');
export function requireValue(condition, code) {
  if (!condition) throw Object.assign(new Error(code), {code});
}
export function requireSourceRevision() {
  requireValue(/^[a-f0-9]{40}$/.test(REVISION) && !/^0{40}$/.test(REVISION), 'EXACT_SOURCE_REVISION_REQUIRED');
}
export function requirePhysicalInputs(value) {
  requireSourceRevision();
  requireValue(/^sha256:[a-f0-9]{64}$/.test(DERIVED_IMAGE_DIGEST) && !/^sha256:0{64}$/.test(DERIVED_IMAGE_DIGEST), 'ACTUAL_DERIVED_IMAGE_DIGEST_REQUIRED');
  requireValue(/^[a-f0-9]{64}$/.test(QUALIFIER_ELF_SHA256) && !/^0{64}$/.test(QUALIFIER_ELF_SHA256), 'ACTUAL_QUALIFIER_ELF_REQUIRED');
  requireValue(/^[a-f0-9]{64}$/.test(SOURCE_TREE_SHA256) && !/^0{64}$/.test(SOURCE_TREE_SHA256), 'ACTUAL_SOURCE_TREE_REQUIRED');
  requireValue(DERIVED_IMAGE_DIGEST !== FORBIDDEN_IMAGE_DIGEST && DERIVED_IMAGE_DIGEST !== BASE_IMAGE_DIGEST, 'PROTECTED_OR_UNDERIVED_IMAGE');
  requireValue(value.image === IMAGE && value.revision === REVISION && value.digest === DERIVED_IMAGE_DIGEST, 'EXACT_DERIVED_PHYSICAL_INPUTS_REQUIRED');
}
const equal = (a,b) => JSON.stringify(a) === JSON.stringify(b);
export function plan(value, started, guestRoot, guestSmoke) {
  requireValue(value.image === IMAGE && value.revision === REVISION, 'WRONG_DERIVED_IMAGE_OR_REVISION');
  requireValue(/^sha256:[a-f0-9]{64}$/.test(value.digest || ''), 'IMMUTABLE_DIGEST_REQUIRED');
  requireValue(value.digest !== FORBIDDEN_IMAGE_DIGEST, 'PROTECTED_HOME_IMAGE');
  const nonce = value.nonce || randomUUID();
  requireValue(/^[a-f0-9]{8}-[a-f0-9]{4}-4[a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12}$/.test(nonce), 'INVALID_NONCE');
  requireValue(Number.isSafeInteger(started) && started > 0, 'INVALID_START_TIME');
  const input = {formatVersion:1, nonce, role:ROLE, revision:REVISION, imageDigest:value.digest,
    packageVersion:VERSION, baseImage:BASE_IMAGE, baseImageDigest:BASE_IMAGE_DIGEST,
    sourceTreeSha256:SOURCE_TREE_SHA256, qualifierElfSha256:QUALIFIER_ELF_SHA256,
    qualifierPath:QUALIFIER_PATH, qualifierTest:QUALIFIER_TEST, scope:'root-ipc-seed-producer-component-only',
    fuseAtMs:started+300000,
    guestSha256:sha(guestRoot.toString()+'\n'+guestSmoke.toString()+'\n'+validateQualifierOutput.toString())};
  const source = `(${guestRoot.toString()})(${JSON.stringify(input)},${guestSmoke.toString()},${validateQualifierOutput.toString()}).catch(()=>{console.error('STOCK_GUEST_BOOTSTRAP_FAILED');process.exitCode=1;});`;
  const config = {image:IMAGE+'@'+value.digest, guest:{cpu_kind:'shared',cpus:2,memory_mb:2048},
    restart:{policy:'no'}, auto_destroy:false, services:[], mounts:[], env:{},
    init:{entrypoint:['/usr/bin/env','-i',...Object.entries(ROOT_FLAGS).map(([k,v])=>k+'='+v),
      '/usr/local/bin/node','--input-type=module','-e',source],cmd:[]},
    metadata:{rudder_role:ROLE,qualification:nonce,qualification_revision:REVISION,
      qualification_image_digest:value.digest,qualification_guest_sha256:input.guestSha256,
      qualification_source_tree_sha256:input.sourceTreeSha256,qualification_elf_sha256:input.qualifierElfSha256,
      qualification_base_image_digest:BASE_IMAGE_DIGEST}};
  requireValue(Buffer.byteLength(JSON.stringify(config))<=65536, 'CONFIG_BYTE_LIMIT');
  return {input, config, name:'rudder-root-storage-ipc-'+nonce, root:'/run/rudder-root-storage-ipc-canary-'+nonce};
}
export function ownership(p) {
  let pinned;
  const empty = value => value === undefined || value === null || Array.isArray(value) && value.length===0;
  const matches = machine => {
    if (!machine || machine.id===FORBIDDEN_ID || machine.instance_id===FORBIDDEN_INSTANCE
        || machine.image_ref?.digest===FORBIDDEN_IMAGE_DIGEST
        || !/^[a-f0-9]{14}$/.test(machine.id || '') || machine.name!==p.name
        || typeof machine.instance_id!=='string' || !machine.instance_id) return false;
    if (pinned && (machine.id!==pinned.id || machine.instance_id!==pinned.instance)) return false;
    if (machine.image_ref?.digest!==p.input.imageDigest) return false;
    const labels = machine.image_ref?.labels;
    if (labels!==undefined && labels!==null && labels['org.opencontainers.image.revision']!==REVISION) return false;
    const config = machine.config;
    if (!config || !Object.entries(p.config.metadata).every(([k,v])=>config.metadata?.[k]===v)) return false;
    if (![p.config.image, IMAGE, 'registry.fly.io/rudder-workers-exla@'+p.input.imageDigest].includes(config.image)) return false;
    if (!['services','mounts','files','volumes'].every(key=>empty(config[key])) || !empty(machine.volumes)) return false;
    if (config.restart?.policy!=='no' || config.auto_destroy===true) return false;
    if (!Object.entries(p.config.guest).every(([k,v])=>config.guest?.[k]===v)) return false;
    if (!config.init || !Object.keys(config.init).every(k=>['entrypoint','cmd'].includes(k))
        || !equal(config.init.entrypoint,p.config.init.entrypoint) || !equal(config.init.cmd??[],p.config.init.cmd)) return false;
    if (!Object.entries(config.env || {}).every(([k,v])=>['PRIMARY_REGION','FLY_PRIMARY_REGION'].includes(k)&&v==='sjc')) return false;
    if (pinned && (labels?.['org.opencontainers.image.revision'] || null)!==pinned.revisionLabel) return false;
    return true;
  };
  return {matches, pinned:()=>pinned, remember(machine) {
    requireValue(matches(machine), 'OWNERSHIP_MISMATCH');
    pinned ??= {id:machine.id,instance:machine.instance_id,digest:machine.image_ref.digest,
      revisionLabel:machine.image_ref.labels?.['org.opencontainers.image.revision'] || null};
    return {...pinned};
  }};
}
