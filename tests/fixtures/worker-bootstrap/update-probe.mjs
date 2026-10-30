import fs from 'node:fs';
const tool=process.argv[2];
const status=fs.readFileSync('/proc/self/status','utf8');
fs.appendFileSync(process.env.BOOT_LOG_PATH,JSON.stringify({tool,uid:process.getuid(),nnp:/^NoNewPrivs:\s*(\d+)/m.exec(status)?.[1]})+'\n');
if(tool==='npm')console.log('0.0.0-fixture');
if(tool==='codex')console.log('codex 0.0.0-fixture');
