import assert from 'node:assert/strict';
import test from 'node:test';
import {pollOriginalStagingReceiptRetaining} from '../cloud/dist/managed-home-provisioning/staging-ordinary-sdk-driver.js';

test('same-key receipt polling covers a 25-second build and its later two-join cleanup',async()=>{
 let elapsed=0,queries=0;const keys=[],stableKey='original-sdk-command';
 await pollOriginalStagingReceiptRetaining({
  current(){assert(elapsed<60000,'original retained restriction expired');},
  async query(){queries++;keys.push(stableKey);return {completed:elapsed>=25000,buildJoined:elapsed>=25500,cleanupJoined:elapsed>=40000};},
  settled(data){return data.completed&&data.buildJoined&&data.cleanupJoined;},
  async wait(milliseconds){assert.equal(milliseconds,500);elapsed+=milliseconds;},
 });
 assert.equal(elapsed,40000);assert.equal(queries,81);assert(keys.every(k=>k===stableKey));
});

test('query-only polling preserves the original deadline and does not renew after expiry',async()=>{
 let elapsed=0,queries=0;
 await assert.rejects(pollOriginalStagingReceiptRetaining({
  current(){if(elapsed>=30000)throw Error('original restriction expired');},
  async query(){queries++;return {completed:false};},
  settled(){return false;},async wait(milliseconds){elapsed+=milliseconds;},
 }),/original restriction expired/);
 assert.equal(elapsed,30000);assert.equal(queries,60);
});
