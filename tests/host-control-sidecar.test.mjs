// Pure fake Root-FD/socket/clock relay tests; no real generation/custody/guest isolation proof.
// Written for Root; not executed by the draft author.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { createHash } from 'node:crypto';
import test from 'node:test';
import {PacketQueue,sameGeneration,canonicalData} from '../cloud/worker/host-control-core.mjs';
const source = readFileSync(new URL('../cloud/worker/host-control.mjs', import.meta.url), 'utf8')
  .replace(/^import .*;\n/gm, '');
const nonce = '99999999-9999-4999-8999-999999999999', initial = '11111111-1111-4111-8111-111111111111';
const generation = '22222222-2222-4222-8222-222222222222', attempt='33333333-3333-4333-8333-333333333333';
const transport=(serial='1',gen=generation,id=attempt)=>({serial,generation:gen,attempt_nonce:id});
const authConstraint=()=>({format_version:1,maximum_milliseconds:5000,boottime_deadline_nanoseconds:'105000000000'});
async function fixture(options = {}) {
  let now = 100000, interval, ws, block = false;
  const writes = [], replies = [], incoming = [], closes = [], sockets = [], files = new Map([
    [4, Buffer.from(JSON.stringify({ version: 1, home_id: 'new-home', incarnation: 'boot-1', cloud_origin: 'https://rudder-cloud-control.fly.dev', ...(options.config ?? {}) }))],
    [5, Buffer.from('a'.repeat(32))],
    [7, Buffer.from(JSON.stringify({ version: 1, nonce, constraint: { format_version: 1, maximum_milliseconds: 30000,
      boottime_deadline_nanoseconds: '130000000000' } }))],
  ]);
  const proc = { argv: ['node', 'host-control.mjs'], env: {}, getuid: () => 1500, getgid: () => 1500, exitCode: 0 };
  const fs = {
    fstatSync: fd => ({ isFile: () => true, uid: 0, gid: 0, nlink: fd === 7 ? 0 : 1, mode: fd === 7 ? 0o100777 : 0o100600, size: files.get(fd).length }),
    readSync: (fd, out) => {
      if (fd === 6) { const b = Buffer.from(`${Math.floor(now / 1000)}.${String(Math.floor(now % 1000 / 10)).padStart(2, '0')} 9.00\n`); b.copy(out); return b.length; }
      const b = fd === 3 ? incoming.shift() : files.get(fd);
      if (!b) throw Object.assign(Error(), { code: 'EAGAIN' }); b.copy(out); return b.length;
    },
    writeSync: (fd, b) => { assert.equal(fd, 3); if (block) throw Object.assign(Error(), { code: 'EAGAIN' }); writes.push(JSON.parse(b)); return b.length; },
    closeSync: fd => closes.push(fd),
  };
  class Socket {
    static OPEN = 1;
    readyState = 1; bufferedAmount=0; handlers = {}; terminated = false;
    constructor(url, opts) { this.url = url; this.options = opts; ws = this; sockets.push(this); }
    on(event, fn) { this.handlers[event] = fn; }
    send(bytes, _opts, callback) { replies.push(JSON.parse(bytes)); callback(); }
    terminate() { this.terminated = true; }
  }
  Object.assign(proc, options.process ?? {});
  new Function('fs', 'createHash', 'randomUUID', 'WebSocket', 'process', 'setInterval', 'clearInterval', 'PacketQueue', 'sameGeneration', 'canonicalData', source)(
    fs, createHash, () => initial, Socket, proc, fn => { interval = fn; return 1; }, () => { interval = undefined; }, PacketQueue, sameGeneration, canonicalData);
  await Promise.resolve();
  return { writes, replies, incoming, closes, sockets, proc, get ws() { return ws; },
    commands() { return writes.filter(v=>v.operation); },
    root(frame) { incoming.push(Buffer.from(JSON.stringify(frame)));interval?.(); },
    activate(meta=transport()) { this.root({kind:"host_transport_connect_v1",...meta,constraint:authConstraint()});this.root({kind:"host_transport_activate_v1",...meta,constraint:authConstraint()});this.tick(); },
    disconnect() { const old=ws;old.handlers.error();old.readyState=3;old.handlers.close();this.tick();return old; },
    tick() { interval?.(); }, clock(ms) { now = ms; }, block(value) { block = value; },
    open() {
      ws.handlers.open();
      if (options.registered !== false) {
        incoming.push(Buffer.from(JSON.stringify({kind:'engine_registration_begin_v1',rpc_id:nonce,packet:{body:{test:'opaque'},mac:'0'.repeat(64)}}))); interval();
        incoming.push(Buffer.from(JSON.stringify({kind:'engine_registration_complete_v1',challenge_id:'challenge-1'}))); interval(); replies.length=0;
        if(options.activate!==false)this.activate();
      }
    },
    request(method = 'managed.startTaskV1', params = {}) {
      ws.handlers.message(Buffer.from(JSON.stringify({ type: 'rpc', id: 'rpc-1', method,
        params: { command_key: 'stable-1', project_alias: 'project', backend: 'codex', model: 'model', profile_id: 'api-profile', text: 'a\nb', ...params } })), false);
    },
  };
}
test('actual script keeps token on WS only and exact new managed DATA on IPC', async () => {
  const f = await fixture(); f.open(); assert.equal(f.writes[0].kind, 'ready'); assert.equal(f.writes[0].session_generation, initial);
  assert(!JSON.stringify(f.writes).includes('a'.repeat(32))); assert.equal(f.ws.options.followRedirects, false);
  assert.equal(f.ws.url, 'wss://rudder-cloud-control.fly.dev/api/rudder/home/new-home/control');
  f.request(); const cmd = f.commands()[0]; assert.equal(cmd.operation.op, 'StartTaskV1'); assert.equal(cmd.operation.text, 'a\nb');
  assert.equal(cmd.command_key, 'stable-1'); assert.equal(cmd.constraint.boottime_deadline_nanoseconds, '120000000000');
  f.incoming.push(Buffer.from(JSON.stringify({ version: 1, session_generation: generation, rpc_id: cmd.rpc_id,
    command_key: cmd.command_key, result: { receipt: 'accepted-data' }, error: null })));
  f.tick(); assert.deepEqual(f.replies[0], { type: 'rpc-result', id: 'rpc-1', result: { receipt: 'accepted-data' } });
});
test('one in-flight original request and EAGAIN preserve the captured cutoff', async () => {
  const f = await fixture(); f.open(); f.block(true); f.request(); f.clock(103000); f.request();
  assert.equal(f.replies[0].error, 'MANAGED_BUSY'); f.block(false); f.tick();
  assert.equal(f.commands()[0].constraint.boottime_deadline_nanoseconds, '120000000000');
});
test('wrong reply binding fences without returning a successful RPC', async () => {
  const f = await fixture(); f.open(); f.request(); f.incoming.push(Buffer.from(JSON.stringify({ version: 1,
    session_generation: generation, rpc_id: 'other-rpc', command_key: 'stable-1', result: { receipt: 'wrong' }, error: null })));
  f.tick(); assert.equal(f.proc.exitCode, 78); assert.equal(f.replies.length, 0); assert(f.ws.terminated);
});
test('expired original request remains DATA without killing stable Root IPC or rebasing reply time', async () => {
  const f=await fixture();f.open();f.request();const original=f.commands()[0];f.clock(120000);f.tick();assert.equal(f.proc.exitCode,0);
  f.root({version:1,session_generation:original.session_generation,rpc_id:original.rpc_id,command_key:original.command_key,result:{receipt:'late-data'},error:null});
  assert.equal(f.commands().length,1);assert.equal(f.replies.length,0);assert(!f.closes.includes(3));assert(!f.ws.terminated);
  assert.equal(original.constraint.boottime_deadline_nanoseconds,'120000000000');
});
test('legacy repos are ignored and unknown RPC cannot start a supervisor', async () => {
  const f = await fixture(); f.open(); f.ws.handlers.message(Buffer.from('{"type":"repos","repos":[{"path":"/workspace/anything"}]}'), false);
  f.request('runs'); assert.equal(f.commands().length, 0); assert.equal(f.replies[0].error, 'MANAGED_METHOD_UNSUPPORTED');
});
test('environment inheritance and wrong control origin are rejected before connecting', async () => {
  const f = await fixture({ process: { env: { NODE_OPTIONS: 'forbidden' } } });
  assert.equal(f.ws, undefined); assert.equal(f.proc.exitCode, 78); assert.equal(f.writes.length, 0);
  const wrong = await fixture({ config: { cloud_origin: "https://rudder.exla.ai" } });
  assert.equal(wrong.ws, undefined); assert.equal(wrong.proc.exitCode, 78); assert.equal(wrong.writes.length, 0);
});

test('registration proof/ACK are opaque and cannot enable commands without original Root completion', async () => {
  const f=await fixture({registered:false});f.open();f.request();assert.equal(f.replies[0].error,'MANAGED_TRANSPORT_AUTHENTICATING');assert.equal(f.writes.length,1);
  const packet={body:{opaque:'signed-only'},mac:'b'.repeat(64)};
  f.incoming.push(Buffer.from(JSON.stringify({kind:'engine_registration_begin_v1',rpc_id:nonce,packet})));f.tick();
  assert.deepEqual(f.replies[1],{type:'engine_registration_begin_v1',rpc_id:nonce,packet});
  f.ws.handlers.message(Buffer.from(JSON.stringify({type:'engine_registration_challenge_v1',rpc_id:nonce,packet})),false);
  assert.deepEqual(f.writes[1],{kind:'engine_registration_challenge_v1',rpc_id:nonce,packet});
  f.ws.handlers.message(Buffer.from(JSON.stringify({type:'engine_registration_ack_v1',rpc_id:nonce,packet})),false);f.request();
  assert.equal(f.replies.at(-1).error,'MANAGED_TRANSPORT_AUTHENTICATING');
  f.incoming.push(Buffer.from(JSON.stringify({kind:'engine_registration_complete_v1',challenge_id:'challenge-1'})));f.tick();f.request();
  assert.equal(f.replies.at(-1).error,'MANAGED_TRANSPORT_AUTHENTICATING');f.activate();f.request();assert.equal(f.commands()[0].operation.op,'StartTaskV1');
});
test('registration request ID cannot replace the original in-flight Root conversation', async () => {
  const f=await fixture({registered:false});f.open();
  f.incoming.push(Buffer.from(JSON.stringify({kind:'engine_registration_begin_v1',rpc_id:nonce,packet:{body:{},mac:'0'.repeat(64)}})));f.tick();
  f.incoming.push(Buffer.from(JSON.stringify({kind:'engine_registration_proof_v1',rpc_id:'other',packet:{body:{},mac:'0'.repeat(64)}})));f.tick();
  assert.equal(f.proc.exitCode,78);assert(f.ws.terminated);
});

test('engine operations and ACKs are opaque only after original Root registration', async () => {
  const f=await fixture();f.open();const packet={body:{request_nonce:'operation-1',command_key:'original',payload_sha256:'b'.repeat(64),constraint:{format_version:1,maximum_milliseconds:30000,boottime_deadline_nanoseconds:'130000000000'},operation:{op:'PrepareFreshProjectV1',workspace_id:'w'}},mac:'b'.repeat(64)};
  f.incoming.push(Buffer.from(JSON.stringify({kind:'engine_operation_v1',rpc_id:'operation-1',packet,transport:transport()})));f.tick();
  assert.deepEqual(f.replies[0],{type:'engine_operation_v1',rpc_id:'operation-1',packet});
  f.ws.handlers.message(Buffer.from(JSON.stringify({type:'engine_operation_ack_v1',rpc_id:'operation-1',packet})),false);
  assert.deepEqual(f.writes.at(-1),{kind:'engine_operation_ack_v1',rpc_id:'operation-1',packet});
});
test('engine operation cannot skip original registered Root completion', async () => {
  const f=await fixture({registered:false});f.open();f.incoming.push(Buffer.from(JSON.stringify({kind:'engine_operation_v1',rpc_id:'operation-1',packet:{body:{},mac:'b'.repeat(64)}})));f.tick();
  assert.equal(f.proc.exitCode,78);assert(f.ws.terminated);
});


test('WS loss retains Root IPC and requires a distinct Root command before replacement TLS or RPC',async()=>{
  const f=await fixture();f.open();const old=f.disconnect();for(let i=0;i<3;i++)f.tick();assert.equal(f.proc.exitCode,0);assert(!f.closes.includes(3));assert.equal(f.sockets.length,1);
  const next=transport('2','44444444-4444-4444-8444-444444444444','55555555-5555-4555-8555-555555555555');
  f.root({kind:'host_transport_connect_v1',...next,constraint:authConstraint()});assert.equal(f.sockets.length,2);f.ws.handlers.open();f.tick();f.request();assert.equal(f.replies.at(-1).error,'MANAGED_TRANSPORT_AUTHENTICATING');
  f.root({kind:'host_transport_activate_v1',...next,constraint:authConstraint()});f.tick();f.request();assert.equal(f.commands()[0].session_generation,next.generation);
  old.handlers.message(Buffer.from(JSON.stringify({type:'engine_operation_ack_v1',rpc_id:'stale',packet:{body:{},mac:'f'.repeat(64)}})),false);assert.equal(f.proc.exitCode,0);assert.equal(f.writes.some(v=>v.rpc_id==='stale'),false);
});
test('known never-forwarded operation returns exact original canonical hash and never self-replays',async()=>{
  const f=await fixture();f.open();f.disconnect();const before=f.replies.length,packet={body:{request_nonce:'op-1',command_key:'original',payload_sha256:'b'.repeat(64),constraint:{format_version:1,maximum_milliseconds:30000,boottime_deadline_nanoseconds:'130000000000'},operation:{op:'PrepareFreshProjectV1',workspace_id:'w'}},mac:'b'.repeat(64)};
  f.root({kind:'engine_operation_v1',rpc_id:'op-1',packet,transport:transport()});f.tick();f.tick();const proof=f.writes.find(v=>v.kind==='engine_operation_not_forwarded_v1');assert.equal(f.replies.length,before);assert.equal(proof.request_sha256,createHash('sha256').update(JSON.stringify(canonicalData(packet.body))).digest('hex'));assert.deepEqual(proof.transport,transport());assert.equal(f.proc.exitCode,0);
});
