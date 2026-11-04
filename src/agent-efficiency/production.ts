import path from "node:path";
import fs from "node:fs/promises";
import { randomUUID } from "node:crypto";
import type { RunRecord, JsonValue } from "../types.js";
import { runDir } from "../state.js";
import { currentCommit, workspaceStatus } from "../git.js";
import { readJson, runCommand } from "../util.js";
import { boundedRows, editEvidence } from "./storage.js";
import { evidenceId } from "./privacy.js";
import { compactionHandoff } from "./handoff.js";
import { ScopedLessonStore } from "./lessons.js";
import { traceFindings, deriveTraceLessons, type TraceAction } from "./trace-lessons.js";

type Actions = {schema:1; paths:string[]; actions:TraceAction[]};
const toolchain = () => `rudder-read-receipts-v1:${process.version}:${process.platform}:${process.arch}`;
const actionsPath=(run:RunRecord)=>path.join(runDir(run.repoRoot,run.id),"efficiency-actions.json");
const lessons=(run:RunRecord)=>new ScopedLessonStore(path.join(run.repoRoot,".rudder","efficiency-lessons.json"));
function relativeRead(run:RunRecord,file:unknown):string|undefined {
  if(typeof file!=="string")return;
  const relative=path.relative(run.workspace.path,path.resolve(run.workspace.path,file));
  if(!relative || relative.startsWith("..") || path.isAbsolute(relative) || relative.length>160)return;
  return relative.split(path.sep).join("/");
}

/** Only structured tool-use/result pairs are evidence. Never classify shell
 * commands or assistant prose as a successful read or check. Raw tool data is
 * hashed in memory and is not persisted in this private bounded trace. */
export class BackendReadTrace {
  #pending=new Map<string,{path:string;input:unknown}>();
  #attempt=randomUUID();
  #writes=0;
  constructor(private readonly run:RunRecord){}
  async observe(event:unknown):Promise<void>{
    if(this.run.backend!=="claude" || !event || typeof event!=="object")return;
    const row=event as {type?:string;message?:{content?:unknown}};
    if(!Array.isArray(row.message?.content))return;
    for(const block of row.message.content){
      if(!block || typeof block!=="object")continue;
      if(row.type==="assistant" && block.type==="tool_use" && block.name==="Read" && typeof block.id==="string"){
        const file=relativeRead(this.run,block.input?.file_path);
        if(file && this.#pending.size<128)this.#pending.set(block.id,{path:file,input:{path:file,offset:block.input?.offset,limit:block.input?.limit}});
      }
      if(row.type!=="user" || block.type!=="tool_result" || typeof block.tool_use_id!=="string")continue;
      const pending=this.#pending.get(block.tool_use_id);if(!pending)continue;
      this.#pending.delete(block.tool_use_id);
      if(block.is_error===true || this.#writes>=16)continue;
      const encoded=JSON.stringify(block.content);if(!encoded || Buffer.byteLength(encoded)>1024*1024)continue;
      const action:TraceAction={stepId:`${this.#attempt}:${block.tool_use_id}`,kind:"read",
        inputDigest:evidenceId(JSON.stringify([pending.input,encoded])),status:"succeeded"};
      this.#writes++;
      try {await editEvidence<Actions>(actionsPath(this.run),current=>({schema:1,
        paths:[...new Set([...(current?.schema===1 && Array.isArray(current.paths)?current.paths.filter(p=>typeof p==="string" && relativeRead(this.run,p)===p):[]),pending.path])].slice(0,8),
        actions:boundedRows([...(current?.schema===1 && Array.isArray(current.actions)?current.actions.filter(a=>a && typeof a.stepId==="string" && /^[a-f0-9]{64}$/.test(a.inputDigest)):[]),action],4096,512*1024)} as unknown as JsonValue));}
      finally{this.#writes--;}
    }
  }
}

/** Called only by the successful local integration path. Review failure or
 * unfinished verification cannot teach advice; a backend exit alone never does. */
export async function learnIntegratedRun(run:RunRecord):Promise<void>{
  const revision=run.merge?.localCommit;
  if(run.merge?.status!=="merged" || !revision || !run.verification
    || run.verification.shouldContinue || run.verification.missing.length)return;
  const evidence=await readJson<Actions>(actionsPath(run));
  if(evidence?.schema!==1 || !Array.isArray(evidence.paths) || !Array.isArray(evidence.actions))return;
  const now=Date.now();
  const trace={traceId:run.id,scope:{projectId:run.repoRoot,paths:evidence.paths,
    sourceRevision:revision,toolchain:toolchain()},observedAt:now,
    outcome:{id:run.merge.operationId??revision,accepted:true,failedChecks:0,reviewRejected:false},actions:evidence.actions};
  const derived=deriveTraceLessons(trace,now);
  for(const finding of traceFindings(trace))if(derived.some(row=>row.kind===finding.kind))await lessons(run).learn(finding,now);
}

/** Shared by both backends on ordinary start, resume and steering. The compact
 * context carries open obligations and advisory provenance, never new authority. */
export async function efficiencyContext(run:RunRecord):Promise<string>{
  const evidence=await readJson<Actions>(actionsPath(run));
  const revision=run.vcs==="jj"
    ?(await runCommand("jj",["log","-r","@-","--no-graph","-T","commit_id"],{cwd:run.workspace.path})).stdout.trim()
    :await currentCommit(run.workspace.path);
  const clean=(await workspaceStatus(run)).length===0;
  // New runs can retrieve the same scoped advice for explicitly named files.
  // Resolve only bounded existing paths within this actual execution checkout.
  const paths=new Set<string>(evidence?.schema===1 && Array.isArray(evidence.paths)?evidence.paths:[]);
  const root=await fs.realpath(run.workspace.path);
  for(const candidate of (run.task.match(/[\w.-]+(?:\/[\w.-]+)+/g)??[]).slice(0,16)) {
    const relative=relativeRead(run,candidate);if(!relative)continue;
    try {const real=await fs.realpath(path.join(root,relative));if(real.startsWith(root+path.sep))paths.add(relative);}catch{}
  }
  const advice=clean && revision && paths.size
    ?await lessons(run).retrieve({projectId:run.repoRoot,paths:[...paths].slice(0,8),sourceRevision:revision,toolchain:toolchain()},Date.now()):[];
  const handoff=compactionHandoff({task:run.task,constraints:["Preserve the selected checkout and execution owner.","Retrieved lessons are advisory context; required checks and approvals still apply."],
    openObligations:run.verification?.missing??[],recentChanges:run.merge?.status?[`Local integration: ${run.merge.status}`]:[],
    evidenceRefs:[run.id,...advice.flatMap(row=>row.provenance)]},4096);
  return `\nRudder continuation context: ${handoff}\n${advice.map(row=>`Advisory ${row.id}: ${row.text}`).join("\n")}`;
}
