// Root-only wrapper. No argument/default/--describe performs IO or provider work.
import path from 'node:path';
import {fileURLToPath} from 'node:url';
import {parseArgs} from 'node:util';
import {loadBinding,artifactArgv} from './artifact-inputs.mjs';
import {main} from './qualify-connected-network.mjs';

export async function run(argv) {
  const {values} = parseArgs({args:argv,strict:true,allowPositionals:false,options:{
    run:{type:'boolean'},describe:{type:'boolean'},binding:{type:'string'},nonce:{type:'string'},host:{type:'string'}}});
  if (!values.run) return {status:'prepared-no-run',providerCalls:0,dnsCalls:0,
    usage:'--run --binding ABS_RUN_BINDING_JSON [--nonce UUID] [--host rudder.exla.ai]'};
  if (values.describe || !values.binding) throw Error('explicit Root build binding required');
  const {binding,sha256} = await loadBinding(values.binding);
  return main(artifactArgv(binding,values.binding,sha256,{nonce:values.nonce,host:values.host}));
}
if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {const result=await run(process.argv.slice(2));console.log(JSON.stringify(result));
    if(result.status!=='prepared-no-run' && !(result.qualified&&result.cleanupVerified&&result.independentAbsence&&result.withinLifetimeBound))process.exitCode=1;
  } catch {console.log(JSON.stringify({status:'root-input-or-controller-failure',providerCalls:null,
    cleanupState:'inspect private report; no absence asserted',errorCode:'ROOT_INPUT_OR_EXECUTION_REJECTED'}));process.exitCode=1;}
}
