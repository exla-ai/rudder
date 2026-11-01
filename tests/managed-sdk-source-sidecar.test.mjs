// Pure fake Root-FD/socket/clock relay tests; no real generation/custody/guest isolation proof.
// Definitions UNRUN. Reuses fake DATA transport only, no real Root/WS authority.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { createHash } from 'node:crypto';
import test from 'node:test';
import {PacketQueue,sameGeneration,canonicalData} from '../cloud/worker/host-control-core.mjs';
import {RuntimeQualificationRelay,READY as QUAL_READY,ACK as QUAL_ACK} from '../cloud/worker/runtime-qualification.mjs';
import {policy,scope} from './fixtures/managed-runtime-qualification/data.mjs';
import {C1_COMPILER_SDK} from '../cloud/dist/managed-home-provisioning/compiler-sdk-catalog.js';
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
  new Function('fs', 'createHash', 'randomUUID', 'WebSocket', 'process', 'setInterval', 'clearInterval', 'PacketQueue', 'sameGeneration', 'canonicalData', 'RuntimeQualificationRelay', 'QUAL_READY', 'QUAL_ACK', source)(
    fs, createHash, () => initial, Socket, proc, fn => { interval = fn; return 1; }, () => { interval = undefined; }, PacketQueue, sameGeneration, canonicalData, RuntimeQualificationRelay, QUAL_READY, QUAL_ACK);
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
    request(method = 'managed.buildCurrentSourceSdkV1', params = {}) {
      ws.handlers.message(Buffer.from(JSON.stringify({ type: 'rpc', id: 'rpc-1', method,
        params: { command_key: 'stable-1', project_alias: 'project', case: 'accepted', repeat_identical: true, ...params } })), false);
    },
  };
}

test('actual sidecar script forwards exact no-model SDK command under retained fake transport generation',async()=>{
 const f=await fixture();f.open();f.request();assert.equal(f.commands().length,1);assert.deepEqual(f.commands()[0].operation,{op:'BuildCurrentSourceSdkV1',project_alias:'project',case:'accepted',repeat_identical:true});
 assert.equal(f.commands()[0].command_key,'stable-1');assert.equal(f.commands()[0].constraint.maximum_milliseconds,20000);
});
test('actual sidecar refuses model/source additions and malformed SDK case/repeat DATA',async()=>{
 for(const params of [{model:'default'},{source_root:'caller'},{case:'unknown'},{repeat_identical:'true'}]){const f=await fixture();f.open();f.request(undefined,params);assert.equal(f.commands().length,0);}
});
test('SDK command cannot cross closed original generation or scoped qualification transport',async()=>{
 const closed=await fixture();closed.open();closed.disconnect();closed.request();assert.equal(closed.commands().length,0);
 const runtime=scope(policy());for(const config of [{runtime_qualification:runtime},{runtime_qualification:runtime,compiler_qualification:{...runtime,scope:'disposable-fixed-compiler-sdk-only',sdk:structuredClone(C1_COMPILER_SDK)}}]){const f=await fixture({config});f.open();f.request();assert.equal(f.commands().length,0);}
});
