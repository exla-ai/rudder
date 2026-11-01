// Pure and closure-free: the SAME function is imported by controller/guest and
// embedded in init with its exact source included in qualification_guest_sha256.
// Validates component observations only. It grants no production authority.
export function validateQualifierOutput(text,test,elfSha256) {
  const need=(ok,code)=>{if(!ok)throw Object.assign(new Error(code),{code});};
  const equal=(a,b)=>JSON.stringify(a)===JSON.stringify(b);
  const integer=v=>Number.isSafeInteger(v)&&v>=0;
  const pid=v=>integer(v)&&v>1;
  const uuid=v=>typeof v==='string'&&/^[a-f0-9]{8}-[a-f0-9]{4}-4[a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12}$/.test(v);
  const hex=(v,n)=>typeof v==='string'&&new RegExp('^[a-f0-9]{'+n+'}$').test(v);
  const seed=v=>v?.kind==='directory'&&v.algorithm==='sha256'&&hex(v.digest,64)&&integer(v.size_bytes)&&v.size_bytes>0&&v.size_bytes<=262144;
  const identity=v=>v&&hex(v.commit,40)&&hex(v.change,32)&&Array.isArray(v.parents)&&v.parents.length<=1024&&v.parents.every(p=>hex(p,40))
    &&Array.isArray(v.tree?.terms)&&v.tree.terms.length===1&&hex(v.tree.terms[0],40)&&Array.isArray(v.tree.labels)&&v.tree.labels.length===0;
  const pair=v=>Array.isArray(v)&&v.length===2&&v.every(integer);
  const subset=(value,required,code)=>{need(value&&typeof value==='object'&&!Array.isArray(value),code);for(const [k,v]of Object.entries(required))need(value[k]===v,code);};
  need(test==='root_storage_service::ipc_qualification_tests::root_storage_ipc_kernel_qualification'&&hex(elfSha256,64),'QUALIFIER_CONTRACT');
  const prefix='test '+test+' ... ',lines=text.split(/\r?\n/);
  need(lines.filter(line=>line.startsWith(prefix)).length===1&&lines.filter(line=>line==='running 1 test').length===1
    &&lines.filter(line=>/^test result: ok\. 1 passed; 0 failed; 0 ignored; 0 measured; [0-9]+ filtered out; finished in [^\r\n]+$/.test(line)).length===1,'QUALIFIER_TEST_NOT_RUN');
  const markers=[];
  for(let line of lines){if(line.startsWith(prefix))line=line.slice(prefix.length);if(!line.startsWith('{'))continue;
    try{const value=JSON.parse(line);if(value.scope==='root-ipc-seed-producer-component-only')markers.push(value);}catch{}}
  need(markers.length===1,'QUALIFIER_MARKER_COUNT');const marker=markers[0];
  subset(marker,{format_version:1,scope:'root-ipc-seed-producer-component-only',
    service_and_engine_actual_pidfds_live:true,service_and_engine_owners_retained_until_vm_teardown:true,
    one_helper_exit_kept_sibling_custody:true,actual_scm_credentials:true,actual_proc_exe_bootstrap_binding:true,
    root_sys_ptrace_and_host_namespace_checks:true,root_and_uid1500_erofs:true,independent_root_cas:true,
    cancelled_error_owners_retained:true,normal_retirement_blocked:true,helper_pipe_eof:true,helpers_successfully_reaped:2,
    normal_cleanup_performed:false,production_launcher:false,spec3:false,backend:false,models:false},'QUALIFIER_SCOPE_OR_CUSTODY');
  need(uuid(marker.nonce)&&marker.pinned_elf_sha256===elfSha256,'QUALIFIER_NONCE_OR_ELF');
  need(pid(marker.root_service_pid)&&pid(marker.engine_pid)&&marker.root_service_pid!==marker.engine_pid,'QUALIFIER_SERVICE_PIDS');
  const helpers=marker.actual_helper_pids;
  need(Array.isArray(helpers)&&helpers.length===2&&helpers.every(pid)&&new Set([...helpers,marker.root_service_pid,marker.engine_pid]).size===4,'QUALIFIER_HELPER_PIDS');
  const root=marker.root_snapshot,engine=marker.engine;
  subset(root,{service_pid:marker.root_service_pid,engine_pid:marker.engine_pid,service_image_sha256:elfSha256,engine_image_sha256:elfSha256,
    normal_retirement_blocked:true,normal_cleanup_performed:false,original_host_namespace_checks:true},'QUALIFIER_ROOT_SNAPSHOT');
  need(integer(root.root_caps)&&[19,21].every(bit=>Math.floor(root.root_caps/(2**bit))%2===1),'QUALIFIER_ROOT_CAPS');
  subset(engine,{cancelled_transfer_failed:true,client_fenced_after_error:true,client_source_pins_retained:2,
    helper_pipe_eof:true,helpers_successfully_reaped:2,normal_retirement_blocked:true,private_forks:2,
    production_template:true,real_git_history:true,second_fork_unchanged:true,source_cas_uid:1500,uid1500_erofs:true},'QUALIFIER_ENGINE_CUSTODY');
  need(integer(engine.source_owner_checks)&&engine.source_owner_checks>0&&seed(engine.seed),'QUALIFIER_ENGINE_SOURCE');
  need(identity(engine.baseline)&&identity(engine.new_baseline)&&engine.new_baseline.commit!==engine.baseline.commit
    &&engine.new_baseline.change!==engine.baseline.change&&!equal(engine.new_baseline.tree,engine.baseline.tree)
    &&equal(engine.new_baseline.parents,[engine.baseline.commit])&&hex(engine.operation,128)&&hex(engine.new_operation,128)
    &&engine.new_operation!==engine.operation,'QUALIFIER_COMMIT_HISTORY');
  need(Array.isArray(root.slots)&&root.slots.length===2&&root.slots.every(s=>uuid(s.slot))&&root.slots[0].slot!==root.slots[1].slot,'QUALIFIER_ROOT_SLOTS');
  const published=root.slots.filter(s=>s.published===true),cancelled=root.slots.filter(s=>s.published===false);
  need(published.length===1&&cancelled.length===1,'QUALIFIER_SLOT_STATES');const live=published[0],failed=cancelled[0];
  subset(live,{published:true,fenced:true,read_checked:true,read_owner_retained:true,mount_owner_retained:true,mount_settled:false,
    root_cas_uid:0,root_cas_gid:0,root_cas_mode:448,import_pin_retained:true,mount_pin_retained:true,cancel_event_retained:true,
    cancel_flag:true,original_budget_cancelled:true,cancel_observer_handle_retained:false,success_observer_actually_joined:true},'QUALIFIER_PUBLISHED_RETAINED');
  subset(failed,{published:false,fenced:true,read_owner_retained:false,mount_owner_retained:false,mount_settled:false,
    root_cas_uid:0,root_cas_gid:0,root_cas_mode:448,import_pin_retained:false,mount_pin_retained:false,stage_owner_retained:true,
    cancel_event_retained:true,cancel_flag:true,original_budget_cancelled:true,cancel_observer_handle_retained:true,
    success_observer_actually_joined:false,root:null,baseline:null,operation:null},'QUALIFIER_CANCELLED_RETAINED');
  need(typeof live.root==='string'&&/^\/run\/rudder-managed-seeds\/[^/]+\/seed$/.test(live.root)
    &&![live.root,live.root_cas,failed.root_cas].some(s=>typeof s==='string'&&s.split('/').some(p=>p==='.'||p==='..'||/[\x00-\x1f\x7f]/.test(p))),'QUALIFIER_ROOT_PATH');
  need(equal(live.seed,engine.seed)&&equal(failed.seed,engine.seed)&&equal(live.baseline,engine.baseline)&&live.operation===engine.operation,'QUALIFIER_EXACT_TRANSFER');
  need([live.root_cas,failed.root_cas].every(s=>typeof s==='string'&&/^\/run\/rudder-root-storage\/[^/]+\/cas$/.test(s))
    &&live.root_cas!==failed.root_cas&&[live.root_cas_identity,failed.root_cas_identity,engine.source_cas_identity].every(pair)
    &&new Set([live.root_cas_identity,failed.root_cas_identity,engine.source_cas_identity].map(value=>JSON.stringify(value))).size===3,'QUALIFIER_INDEPENDENT_CAS');
  need(Array.isArray(live.helpers)&&live.helpers.length===2&&live.helpers.every(h=>uuid(h.reader)&&pid(h.pid)&&h.exited===true)
    &&live.helpers[0].reader!==live.helpers[1].reader&&new Set(live.helpers.map(h=>h.pid)).size===2
    &&helpers.every(p=>live.helpers.some(h=>h.pid===p))&&Array.isArray(failed.helpers)&&failed.helpers.length===0,'QUALIFIER_EXACT_HELPER_EXITS');
  return marker;
}
