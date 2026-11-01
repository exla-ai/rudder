// UNRUN. Exercises the actual Cloud manifest admission function used before
// publication; file bytes/closure and qualified image pin remain independent.
import test from 'node:test';
import assert from 'node:assert/strict';
import {imageManifestPathAfter} from '../cloud/dist/managed-home-provisioning/image-import-policy.js';

test('actual image builder public static inputs are admitted without general etc access',()=>{
 for(const path of ['/usr/lib/a','/opt/rudder/dist/native/rudder-native','/opt/rudder-managed-runtime/v1/manifest.json','/etc/alternatives/editor','/etc/ssl/certs/test.pem','/etc/ld.so.cache','/etc/localtime','/etc/ssl/openssl.cnf','/etc/python3.11/sitecustomize.py'])
  assert.deepEqual(imageManifestPathAfter(path,Buffer.alloc(0)),Buffer.from(path));
 for(const path of ['/etc/shadow','/etc/ssl/private/key','/etc/ssl/openssl.cnf.extra','/etc/python3.11/sitecustomize.py/key','/etc/ld.so.cache.extra','/etc/localtime/key','/etc/ssl/certs/../private/key','/usr//x','/usr/x\0y','/usr/\ud800'])
  assert.throws(()=>imageManifestPathAfter(path,Buffer.alloc(0)));
});
test('manifest ordering follows exact UTF8 bytes rather than UTF16 code units',()=>{
 const bmp='/usr/\ue000',supplementary='/usr/\u{10000}';
 assert(supplementary<bmp);assert(Buffer.compare(Buffer.from(bmp),Buffer.from(supplementary))<0);
 const first=imageManifestPathAfter(bmp,Buffer.alloc(0));imageManifestPathAfter(supplementary,first);
 assert.throws(()=>imageManifestPathAfter(bmp,Buffer.from(supplementary)));
 assert.throws(()=>imageManifestPathAfter(bmp,first));
});
