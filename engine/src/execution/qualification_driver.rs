// Explicit fixture binary, built by Cargo in the same graph as rudder-native.
// No namespace or device operations occur without an explicit qualification mode.
fn main() {
    if std::env::args().nth(1).as_deref() == Some("--describe") {
        println!("{{\"kind\":\"rudder-execution-qualifier\",\"format_version\":1,\"linux\":{},\"default_launch_changed\":false}}", cfg!(target_os = "linux"));
        return;
    }
    #[cfg(target_os = "linux")]
    linux_qualifier::main();
    #[cfg(not(target_os = "linux"))]
    {
        eprintln!("execution qualification requires an explicitly owned Linux worker");
        std::process::exit(2);
    }
}
#[cfg(target_os = "linux")]
mod linux_qualifier {
    // Standalone public-fixture controller, compiled explicitly by execution_mount.
    // This file is not a production engine module and contains no real credentials.
    use rudder_engine::{
        agent_api::AgentIdentityFile,
        execution::{
            handoff::ExecutionHandoff, BrokerBinding, ExecutionSpec, HomeFile, ReadOnlyRuntime,
        },
        journal::{CapabilityGrant, Journal},
        protocol::{ObjectRef, OwnerEpoch},
        store::{DirectoryEntry, DirectoryManifest, EntryNode, LocalStore},
        vfs::{VfsCheckpoint, VfsManager},
    };
    use rudder_native::pty_terminal::{TerminalCommand, TerminalPane, TerminalPaneOptions};
    use std::{
        collections::{BTreeMap, BTreeSet},
        fs,
        os::{
            fd::{AsRawFd, FromRawFd, OwnedFd},
            unix::{fs::PermissionsExt, process::CommandExt},
        },
        path::{Path, PathBuf},
        process::{Command, Stdio},
        time::{Duration, Instant},
    };
    use uuid::Uuid;

    const NODE: &str = r#"
const fs=require('fs'),cp=require('child_process'),crypto=require('crypto');
const hash=x=>crypto.createHash('sha256').update(x).digest('hex');
const denied=f=>{let failed=false;try{f()}catch{failed=true}if(!failed)throw Error('forbidden access succeeded')};
if(process.env.RUDDER_HOST_SHOULD_NOT_LEAK)throw Error('inherited host environment leaked');
const status=fs.readFileSync('/proc/self/status','utf8');if(!/CapEff:\s+0000000000000000/.test(status)||!/NoNewPrivs:\s+1/.test(status))throw Error('capabilities or no-new-privs invalid');
for(const p of JSON.parse(process.env.FORBIDDEN_PATHS)){denied(()=>fs.readFileSync(p));denied(()=>fs.readFileSync('/proc/1/root'+p));denied(()=>fs.writeFileSync(p,'forbidden mutation'))}
denied(()=>fs.readdirSync('/proc/1/fd'));denied(()=>fs.readFileSync('/etc/shadow'));denied(()=>fs.writeFileSync('/usr/rudder-forbidden','bad'));denied(()=>fs.readFileSync('/dev/fuse'));
for(const n of fs.readdirSync('/proc/self/fd')){let target='';try{target=fs.readlinkSync('/proc/self/fd/'+n)}catch{}if(target.includes(process.env.HOST_FIXTURE))throw Error('host authority FD leaked')}
const own=fs.readFileSync('/home/agent/own-auth','utf8');fs.writeFileSync('/workspace/credential-proof.json',JSON.stringify({home:hash(own),env:hash(process.env.OWN_API_KEY)}));
fs.writeFileSync('/tmp/private-temp','temp');fs.writeFileSync('/scratch/private-scratch','scratch');
cp.execFileSync('/usr/bin/gcc',['/workspace/hello.c','-o','/workspace/hello']);if(cp.execFileSync('/workspace/hello',{encoding:'utf8'})!=='compile-ok\n')throw Error('compiled program failed');
let identity=JSON.parse(fs.readFileSync('/run/rudder/agent/identity.json','utf8'));denied(()=>fs.writeFileSync('/run/rudder/agent/identity.json','changed'));let init='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"execution-canary","version":"1"}}}\n';
let mcp=cp.spawnSync('/opt/runtime/rudder-native',['fabric','mcp','--broker-socket','/run/rudder/agent/broker.sock','--identity-file','/run/rudder/agent/identity.json'],{input:init,encoding:'utf8',timeout:10000});if(mcp.status!==0||!mcp.stdout.includes('protocolVersion'))throw Error('valid broker MCP failed: '+mcp.stderr);
identity.token='forged-token-must-be-rejected';fs.writeFileSync('/workspace/forged-identity.json',JSON.stringify(identity),{mode:0o600});mcp=cp.spawnSync('/opt/runtime/rudder-native',['fabric','mcp','--broker-socket','/run/rudder/agent/broker.sock','--identity-file','/workspace/forged-identity.json'],{input:'',encoding:'utf8',timeout:10000});if(mcp.status===0)throw Error('forged broker identity was accepted');
cp.execFileSync('/usr/bin/gcc',['/workspace/async.c','-o','/workspace/async']);cp.execFileSync('/workspace/async');
process.stdout.write('node-compiler-broker-complete Ω😀\u001b[31mRED\u001b[0m\n');
"#;
    const C: &str = "#include <stdio.h>\nint main(void){puts(\"compile-ok\");return 0;}\n";
    const ASYNC: &str = r#"
#define _GNU_SOURCE
#include <linux/io_uring.h>
#include <sys/syscall.h>
#include <sys/mman.h>
#include <unistd.h>
#include <fcntl.h>
#include <string.h>
#include <stdlib.h>
int main(void){struct io_uring_params p={0};int ring=syscall(__NR_io_uring_setup,8,&p);if(ring<0)return 11;size_t size=p.sq_off.array+p.sq_entries*sizeof(unsigned);void* sq=mmap(0,size,PROT_READ|PROT_WRITE,MAP_SHARED|MAP_POPULATE,ring,IORING_OFF_SQ_RING);struct io_uring_sqe* e=mmap(0,p.sq_entries*sizeof(*e),PROT_READ|PROT_WRITE,MAP_SHARED|MAP_POPULATE,ring,IORING_OFF_SQES);if(sq==MAP_FAILED||e==MAP_FAILED)return 12;int fd=open("/workspace/async-write",O_CREAT|O_WRONLY|O_TRUNC,0600);if(fd<0)return 13;char* b=malloc(4096);memset(b,'Q',4096);memset(&e[0],0,sizeof(e[0]));e[0].opcode=IORING_OP_WRITE;e[0].flags=IOSQE_ASYNC;e[0].fd=fd;e[0].addr=(unsigned long)b;e[0].len=4096;e[0].user_data=7;unsigned* array=(unsigned*)((char*)sq+p.sq_off.array);array[0]=0;__atomic_store_n((unsigned*)((char*)sq+p.sq_off.tail),1,__ATOMIC_RELEASE);if(syscall(__NR_io_uring_enter,ring,1,0,0,0,0)!=1)return 14;usleep(1000);return 0;}
"#;
    const WRITER: &str = r#"const fs=require('fs');process.stdout.write('');fs.writeFileSync('/workspace/writer-ready','ready');setInterval(()=>{fs.appendFileSync('/workspace/writer-log','writer\n');fs.appendFileSync('/scratch/writer-log','writer\n')},5);"#;
    const WAIT: &str = r#"const fs=require('fs'),cp=require('child_process');cp.spawn('/usr/bin/node',['/workspace/writer.js',process.argv[2]],{detached:true,stdio:'ignore'}).unref();const t=setInterval(()=>{if(fs.existsSync('/workspace/writer-ready')){clearInterval(t);process.stdout.write('detached-writer-ready Ω😀\n');process.stdin.resume();process.stdin.once('data',()=>process.exit(0));}},5);"#;

    fn dir(store: &LocalStore, files: &[(&str, &[u8])]) -> ObjectRef {
        store
            .put_directory(DirectoryManifest {
                format_version: 1,
                mode: 0o755,
                entries: files
                    .iter()
                    .map(|(n, b)| {
                        DirectoryEntry::new(
                            n.as_bytes(),
                            EntryNode::File {
                                object: store.put_blob(b).unwrap(),
                                mode: 0o644,
                            },
                        )
                        .unwrap()
                    })
                    .collect(),
            })
            .unwrap()
    }
    fn private(path: &Path) {
        fs::create_dir_all(path).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    fn proc_ppid(pid: u32) -> Option<u32> {
        fs::read_to_string(format!("/proc/{pid}/status"))
            .ok()?
            .lines()
            .find_map(|l| l.strip_prefix("PPid:").and_then(|p| p.trim().parse().ok()))
    }
    fn descendants(root: u32) -> Vec<u32> {
        let mut known = BTreeSet::from([root]);
        loop {
            let mut changed = false;
            for e in fs::read_dir("/proc").unwrap().filter_map(|e| e.ok()) {
                if let Ok(pid) = e.file_name().to_string_lossy().parse::<u32>() {
                    if proc_ppid(pid).is_some_and(|p| known.contains(&p)) {
                        changed |= known.insert(pid);
                    }
                }
            }
            if !changed {
                break;
            }
        }
        known.into_iter().collect()
    }
    fn pidfds(pids: Vec<u32>) -> Vec<OwnedFd> {
        pids.into_iter()
            .filter_map(|pid| {
                let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
                if fd < 0 {
                    None
                } else {
                    Some(unsafe { OwnedFd::from_raw_fd(fd as i32) })
                }
            })
            .collect()
    }
    fn exited(fd: &OwnedFd) -> bool {
        let mut p = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        (unsafe { libc::poll(&mut p, 1, 0) }) == 1
    }
    fn run(
        spec: &ExecutionSpec,
        native: &str,
        mode: &str,
        mut start_broker: Option<Box<dyn FnMut()>>,
    ) -> (serde_json::Value, Vec<u8>, usize) {
        let h = ExecutionHandoff::new(spec).unwrap();
        let command = TerminalCommand::with_args(native, h.helper_args())
            .with_env("RUDDER_HOST_SHOULD_NOT_LEAK", "host-environment-marker");
        let mut pane =
            TerminalPane::spawn_shell_or_command(Some(command), TerminalPaneOptions::default())
                .unwrap();
        let pid = pane.child_process_id().unwrap();
        let h = h.activate(pid).unwrap();
        if let Some(start) = start_broker.as_mut() {
            start();
        }
        let mut bytes = Vec::new();
        let mut witnesses = Vec::new();
        let mut acted = false;
        let deadline = Instant::now() + Duration::from_secs(60);
        while pane.is_alive() {
            bytes.extend(pane.drain_output());
            let text = String::from_utf8_lossy(&bytes);
            if !acted
                && ((mode == "interrupt" && text.contains("interrupt-ready"))
                    || (mode != "interrupt" && text.contains("detached-writer-ready")))
            {
                acted = true;
                witnesses = pidfds(descendants(pid));
                if mode == "crash" {
                    assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGKILL) }, 0);
                } else if mode == "interrupt" {
                    pane.write_input(&[3]).unwrap();
                } else {
                    pane.write_input(b"finish\n").unwrap();
                }
            }
            assert!(
                Instant::now() < deadline,
                "execution helper deadline: {}",
                String::from_utf8_lossy(&bytes)
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        bytes.extend(pane.drain_output());
        h.wait(Duration::from_secs(2)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !witnesses.iter().all(exited) {
            assert!(
                Instant::now() < deadline,
                "execution left a witnessed process alive"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let record: serde_json::Value = serde_json::from_slice(
            &fs::read(
                spec.execution_root
                    .join("runs")
                    .join(&spec.execution_id)
                    .join("record.json"),
            )
            .unwrap(),
        )
        .unwrap();
        (record["record"].clone(), bytes, witnesses.len())
    }

    pub fn main() {
        let args = std::env::args().collect::<Vec<_>>();
        if args[1] == "rpc-as-user" {
            unsafe {
                assert_eq!(libc::setgroups(0, std::ptr::null()), 0);
                assert_eq!(libc::setgid(1500), 0);
                assert_eq!(libc::setuid(1500), 0);
            }
            panic!(
                "fresh UID exec failed: {}",
                Command::new(std::env::current_exe().unwrap())
                    .args(["rpc-qualify", &args[2], &args[3]])
                    .exec()
            );
        }
        if args[1] == "rpc-qualify" {
            qualify_workspace_rpc(&args[2], Path::new(&args[3]));
            return;
        }
        if args[1] == "as-user" {
            unsafe {
                assert_eq!(libc::setgroups(0, std::ptr::null()), 0);
                assert_eq!(libc::setgid(1500), 0);
                assert_eq!(libc::setuid(1500), 0);
            }
            panic!(
                "fresh UID exec failed: {}",
                Command::new(std::env::current_exe().unwrap())
                    .args(["qualify", &args[2], &args[3]])
                    .exec()
            );
        }
        let native = args[2].clone();
        let root = PathBuf::from(&args[3]);
        private(&root);
        let store = LocalStore::open(root.join("cas")).unwrap();
        let manager = VfsManager::open(root.join("vfs"), store.clone()).unwrap();
        let owner = OwnerEpoch {
            owner: "execution-canary".into(),
            epoch: 1,
        };
        let base = dir(
            &store,
            &[
                ("task.js", NODE.as_bytes()),
                ("hello.c", C.as_bytes()),
                ("async.c", ASYNC.as_bytes()),
                ("writer.js", WRITER.as_bytes()),
                ("wait.js", WAIT.as_bytes()),
            ],
        );
        manager.prepare(&base).unwrap();
        let authority = root.join("authority");
        private(&authority);
        let peer = root.join("peer");
        private(&peer);
        fs::write(peer.join("credential"), Uuid::new_v4().to_string()).unwrap();
        let journal_path = authority.join("journal.sqlite");
        let mut journal = Journal::open(&journal_path).unwrap();
        let fence = journal
            .create_project("execution-project", "execution-canary", "base", 0)
            .unwrap();
        journal.register_run(&fence, "own-run", 0).unwrap();
        let grant = CapabilityGrant {
            principal: "own-agent".into(),
            run_id: "own-run".into(),
            run_epoch: 1,
            operations: BTreeSet::from(["agents:find".into(), "mailbox:read".into()]),
            recipients: BTreeMap::from([("own-run".into(), 1)]),
            scopes: BTreeSet::from(["project".into()]),
            topics: BTreeSet::new(),
            expires_at_ms: None,
        };
        let identity = AgentIdentityFile {
            format_version: 1,
            project_id: "execution-project".into(),
            run_id: "own-run".into(),
            run_epoch: 1,
            owner_epoch: 1,
            token: journal.issue_capability(&fence, &grant, 0).unwrap(),
        };
        let projection = root.join("projection");
        private(&projection);
        fs::write(
            projection.join("identity.json"),
            serde_json::to_vec(&identity).unwrap(),
        )
        .unwrap();
        fs::set_permissions(
            projection.join("identity.json"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        let home_secret = Uuid::new_v4().to_string();
        let env_secret = Uuid::new_v4().to_string();
        let forbidden = vec![
            journal_path.clone(),
            peer.join("credential"),
            store.object_path(&base).unwrap(),
        ];
        let mut records = Vec::new();
        for mode in ["static", "node", "teardown", "interrupt", "crash"] {
            let view = Uuid::new_v4().to_string();
            drop(manager.fork(&view, &base, owner.clone()).unwrap());
            let command=match mode{"static"=>vec!["/usr/bin/busybox".into(),"sh".into(),"-ec".into(),"stty -echo -onlcr; test -r /workspace/task.js; test ! -e /root; test ! -e /dev/fuse; printf 'static-shell Ω😀\\033[31mRED\\033[0m\\n'".into()],"node"=>vec!["/usr/bin/node".into(),"/workspace/task.js".into()],"interrupt"=>vec!["/bin/sh".into(),"-ec".into(),"stty -echo -onlcr; printf 'interrupt-ready Ω😀\\n'; exec /usr/bin/sleep 30".into()],_=>vec!["/usr/bin/node".into(),"/workspace/wait.js".into(),Uuid::new_v4().to_string()]};
            let spec = ExecutionSpec {
                profile: None,
                format_version: 1,
                execution_id: Uuid::new_v4().to_string(),
                operation_id: Uuid::new_v4().to_string(),
                execution_root: root.join("executions"),
                manager_root: manager.root().into(),
                store_root: store.root().into(),
                authority_roots: vec![authority.clone(), peer.clone()],
                view_id: view.clone(),
                owner: owner.clone(),
                command,
                environment: BTreeMap::from([
                    ("OWN_API_KEY".into(), env_secret.clone()),
                    (
                        "FORBIDDEN_PATHS".into(),
                        serde_json::to_string(&forbidden).unwrap(),
                    ),
                    ("HOST_FIXTURE".into(), root.to_string_lossy().into_owned()),
                ]),
                runtime: vec![
                    ReadOnlyRuntime {
                        source: "/usr".into(),
                        guest_path: "/usr".into(),
                    },
                    ReadOnlyRuntime {
                        source: PathBuf::from(&native),
                        guest_path: "/opt/runtime/rudder-native".into(),
                    },
                ],
                home_files: vec![HomeFile {
                    relative_path: "own-auth".into(),
                    contents: home_secret.as_bytes().to_vec(),
                }],
                broker_directory: if mode == "node" {
                    Some(projection.clone())
                } else {
                    None
                },
                broker_binding: if mode == "node" {
                    Some(BrokerBinding {
                        project_id: identity.project_id.clone(),
                        run_id: identity.run_id.clone(),
                        run_epoch: 1,
                        owner_epoch: 1,
                    })
                } else {
                    None
                },
                timeout_seconds: 30,
                workspace_observation: None,
            };
            let mut broker = None;
            if mode == "node" {
                broker = Some(
                    Command::new(&native)
                        .args([
                            "fabric",
                            "broker",
                            "--journal",
                            journal_path.to_str().unwrap(),
                            "--identity-file",
                            projection.join("identity.json").to_str().unwrap(),
                            "--socket",
                            projection.join("broker.sock").to_str().unwrap(),
                        ])
                        .stdin(Stdio::piped())
                        .stdout(Stdio::piped())
                        .stderr(Stdio::piped())
                        .spawn()
                        .unwrap(),
                );
                let deadline = Instant::now() + Duration::from_secs(5);
                while !projection.join("broker.sock").exists() {
                    let process = broker.as_mut().unwrap();
                    if process.try_wait().unwrap().is_some() {
                        use std::io::Read;
                        let mut error = String::new();
                        process
                            .stderr
                            .take()
                            .unwrap()
                            .take(4096)
                            .read_to_string(&mut error)
                            .unwrap();
                        panic!("qualification broker startup failed: {error}");
                    }
                    assert!(
                        Instant::now() < deadline,
                        "qualification broker startup deadline"
                    );
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            let (record, bytes, witnesses) = run(&spec, &native, mode, None);
            let text = String::from_utf8(bytes.clone()).unwrap();
            assert!(
                !text.contains(&env_secret)
                    && !text.contains(&home_secret)
                    && !text.contains(&identity.token),
                "credential appeared in PTY output"
            );
            if mode == "crash" {
                assert_ne!(record["phase"], "checkpointed");
                assert!(record["checkpoint"].is_null());
                assert!(witnesses >= 3);
                let before = record.clone();
                let retry = ExecutionHandoff::new(&spec).unwrap();
                drop(retry);
                records.push(serde_json::json!({"mode":mode,"phase":before["phase"],"no_forged_checkpoint":true,"witnessed_processes_exited":witnesses}));
            } else {
                assert_eq!(record["phase"], "checkpointed", "{mode}: {text}");
                assert_eq!(
                    record["agent_exit"],
                    if mode == "interrupt" { 130 } else { 0 },
                    "{mode}: {text}"
                );
                let snapshot: VfsCheckpoint = match serde_json::from_str::<rudder_engine::execution::ExecutionRecord>(&record.to_string()).unwrap() {
                    rudder_engine::execution::ExecutionRecord::V1(record) => record.checkpoint.expect("legacy qualifier requires complete local checkpoint"),
                    rudder_engine::execution::ExecutionRecord::V2(_) => panic!("legacy qualifier cannot consume Record2 or partial checkpoint"),
                };
                if mode == "static" || mode == "node" {
                    assert!(
                        bytes
                            .windows(b"\x1b[31mRED\x1b[0m".len())
                            .any(|w| w == b"\x1b[31mRED\x1b[0m"),
                        "ANSI bytes missing: {text}"
                    );
                    assert!(text.contains("Ω😀"));
                }
                if mode == "node" {
                    let output = root.join("node-result");
                    store.export_tree(&snapshot.directory, &output).unwrap();
                    let proof: serde_json::Value = serde_json::from_slice(
                        &fs::read(output.join("credential-proof.json")).unwrap(),
                    )
                    .unwrap();
                    assert_eq!(proof["home"], hex_digest(home_secret.as_bytes()));
                    assert_eq!(proof["env"], hex_digest(env_secret.as_bytes()));
                    let data = fs::read(output.join("async-write")).unwrap();
                    assert!(data.is_empty() || data == vec![b'Q'; 4096]);
                    assert_eq!(
                        fs::read(output.join("forged-identity.json"))
                            .unwrap()
                            .windows(identity.token.len())
                            .any(|w| w == identity.token.as_bytes()),
                        false
                    );
                    for reference in store.closure(&snapshot.directory).unwrap() {
                        if reference.kind == rudder_engine::protocol::ObjectKind::Blob {
                            let data = store.read_object(&reference).unwrap();
                            for secret in [&home_secret, &env_secret, &identity.token] {
                                assert!(
                                    !data.windows(secret.len()).any(|w| w == secret.as_bytes()),
                                    "credential entered source CAS"
                                );
                            }
                        }
                    }
                }
                if mode == "teardown" {
                    assert!(witnesses >= 3);
                }
                records.push(serde_json::json!({"mode":mode,"phase":record["phase"],"agent_exit":record["agent_exit"],"witnessed_processes_exited":witnesses,"checkpoint":snapshot}));
            }
            if let Some(mut process) = broker {
                drop(process.stdin.take());
                let deadline = Instant::now() + Duration::from_secs(5);
                while process.try_wait().unwrap().is_none() {
                    if Instant::now() > deadline {
                        process.kill().unwrap();
                        process.wait().unwrap();
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            println!("{}", serde_json::json!({"execution_case_passed":mode}));
        }
        assert!(journal_path.exists());
        assert!(fs::read(peer.join("credential")).unwrap().len() > 0);
        println!(
            "{}",
            serde_json::json!({"actual_terminal_pane":true,"profile_uid":unsafe{libc::geteuid()},"read_only_runtime":true,"namespace_proc_fd_denial":true,"selected_home_env_only":true,"no_source_cas_credentials":true,"valid_mcp_and_forged_token_denial":true,"static_shell_node_cc":true,"network_isolated":false,"cgroup_limited":false,"records":records})
        );
    }
    fn hex_digest(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        format!("{:x}", Sha256::digest(bytes))
    }

    const WORKSPACE_RPC_GUEST: &str = r#"
const fs=require('fs'),net=require('net'),crypto=require('crypto');
const uuid=()=>crypto.randomUUID(), assert=(v,m)=>{if(!v)throw Error(m)};
const identity=JSON.parse(fs.readFileSync(process.env.RUDDER_WORKSPACE_IDENTITY,'utf8'));
const expected=JSON.parse(process.env.RUDDER_WORKSPACE_BINDING);
assert(JSON.stringify(identity.binding)===JSON.stringify(expected),'sealed workspace binding mismatch');
function call(operation, offered=identity) {return new Promise((resolve,reject)=>{
 const id=uuid(), body=Buffer.from(JSON.stringify({format_version:1,id,identity:offered,operation}));
 const prefix=Buffer.alloc(4);prefix.writeUInt32BE(body.length);
 const socket=net.createConnection(process.env.RUDDER_WORKSPACE_SOCKET);let data=Buffer.alloc(0),done=false;
 const finish=(error,result)=>{if(done)return;done=true;socket.destroy();error?reject(error):resolve(result)};
 socket.setTimeout(6000,()=>finish(Error('workspace RPC deadline')));
 socket.on('error',()=>finish(Error('workspace RPC transport rejected')));
 socket.on('end',()=>finish(Error('workspace RPC ended without a receipt')));
 socket.on('connect',()=>socket.end(Buffer.concat([prefix,body])));
 socket.on('data',chunk=>{data=Buffer.concat([data,chunk]);if(data.length>262148)return finish(Error('oversize workspace response'));
  if(data.length<4)return;const n=data.readUInt32BE(0);if(n>262144)return finish(Error('oversize frame'));
  if(data.length<n+4)return;let r;try{r=JSON.parse(data.subarray(4,4+n).toString('utf8'))}catch{return finish(Error('invalid workspace frame'))}
  if(r.id!==id||r.format_version!==1)return finish(Error('workspace response binding mismatch'));
  if(r.error){const e=Error(r.error.message);e.code=r.error.code;return finish(e)}finish(null,r.result);
 });
})}
async function denied(operation, offered) {try{await call(operation,offered);throw Error('operation unexpectedly succeeded')}catch(e){if(e.message==='operation unexpectedly succeeded')throw e;return e.code||'transport-rejected'}}
(async()=>{
 assert(process.env.PATH.split(':').includes('/usr/local/bin'),'managed default PATH omitted public /usr/local/bin');
 let readonly=false;try{fs.writeFileSync(process.env.RUDDER_WORKSPACE_IDENTITY,'changed')}catch{readonly=true}assert(readonly,'workspace identity projection writable');
 for(const p of JSON.parse(process.env.FORBIDDEN_PATHS)){let inaccessible=false;try{fs.readFileSync(p)}catch{inaccessible=true}assert(inaccessible,'host authority visible')}
 const fd=fs.openSync('/workspace/live.txt','w+');fs.writeSync(fd,Buffer.from('first'),0,5,0);fs.fsyncSync(fd);
 fs.mkdirSync('/workspace/node_modules');fs.writeFileSync('/workspace/node_modules/prepared','excluded');
 const before=(await call({operation:'cursor'})).value;
 const akey=uuid(),a=(await call({operation:'observe',nonce:akey})).value;
 assert(a.retained&&a.observation.consistency==='live_best_effort','observation did not remain live best effort');
 assert(a.observation.begin.namespace===expected.namespace,'wrong view namespace');
 fs.writeSync(fd,Buffer.from('later'),0,5,0);fs.fsyncSync(fd);
 const bkey=uuid(),b=(await call({operation:'observe',nonce:bkey})).value;
 assert(b.observation.end.generation>a.observation.end.generation,'logical write generation did not advance');
 const again=(await call({operation:'observe',nonce:akey})).value;
 assert(JSON.stringify(again)===JSON.stringify(a),'nonce retry changed original observation');
 const dirty=(await call({operation:'dirty',after:before,limit:128})).value;
 assert(dirty.events.length>0&&!dirty.cursor_lost,'dirty history missing');
 const foreign=await denied({operation:'inspect',nonce:process.env.FOREIGN_NONCE});
 await denied({operation:'release',nonce:process.env.FOREIGN_NONCE});
 const forged={...identity,token:'f'.repeat(64)};assert(await denied({operation:'cursor'},forged)==='unauthenticated','forged identity accepted');
 const unsupported=await denied({operation:'adopt',operation_id:uuid(),expected_current:null,nonce:bkey,evidence:{state_sha256:'a'.repeat(64),tree:{fsynced:true}}});
 assert(unsupported==='unsupported','peer boolean acquired durable WC authority');
 assert((await call({operation:'inspect',nonce:bkey})).value.retained,'unsupported adoption lost pin');
 const seal=await denied({operation:'seal'});
 if(process.env.RPC_CASE!=='live'){
  process.stdout.write('workspace-rpc-ready\n');
  setInterval(()=>{try{fs.writeSync(fd,Buffer.from('later'),0,5,0)}catch{}},5);
  setInterval(()=>call({operation:'cursor'}).catch(()=>process.stdout.write('workspace-rpc-denied\n')),100);
  return;
 }
 await call({operation:'release',nonce:akey});await call({operation:'release',nonce:akey});
 assert(!(await call({operation:'inspect',nonce:akey})).value.retained,'retired pin still acknowledged');
 let gitProof=null;
 if(process.env.RPC_GIT_REQUIRED==='1'){
  const cp=require('child_process'),run=(tool,args)=>cp.execFileSync(tool,args,{encoding:'utf8',timeout:15000,stdio:['ignore','pipe','pipe']});
  const gitVersion=run('git',['--version']).trim(),jjVersion=run('jj',['--version']).trim();
  assert(gitVersion==='git version 2.56.0'&&jjVersion.startsWith('jj 0.40.0'),'unexpected pinned Git/jj');
  run('git',['init','--initial-branch=main','/workspace/git-source']);
  run('git',['-C','/workspace/git-source','-c','user.name=Rudder fixture','-c','user.email=fixture@example.invalid','commit','--allow-empty','-m','first']);
  run('git',['clone','--bare','/workspace/git-source','/workspace/git-remote.git']);
  run('git',['-C','/workspace/git-source','fetch','--porcelain','/workspace/git-remote.git']);
  run('jj',['git','init','--colocate','/workspace/jj-fetch']);
  run('jj',['--repository','/workspace/jj-fetch','git','remote','add','origin','/workspace/git-remote.git']);
  run('jj',['--repository','/workspace/jj-fetch','git','fetch']);
  run('git',['-C','/workspace/git-source','-c','user.name=Rudder fixture','-c','user.email=fixture@example.invalid','commit','--allow-empty','-m','second']);
  run('git',['-C','/workspace/git-source','push','/workspace/git-remote.git','main']);
  run('jj',['--repository','/workspace/jj-fetch','git','fetch']);
  const imported=run('jj',['--repository','/workspace/jj-fetch','log','--no-graph','-r','main@origin','-T','description']);
  assert(imported.includes('second'),'jj fetch did not import changed local remote');
  gitProof={git:gitVersion,jj:jjVersion,fetch_porcelain:true,jj_fetch_changed_file_remote:true,public_usr_local_path:true};
 }
 fs.writeFileSync('/workspace/rpc-proof.json',JSON.stringify({namespace:expected.namespace,profile:expected.profile,first:a.observation,second:b.observation,foreign,unsupported,seal,open_fd_during_observe:true,readonly_projection:true,git:gitProof}));
 fs.closeSync(fd);process.stdout.write('workspace-rpc-live-complete Ω😀\n');
})().catch(error=>{process.stderr.write('workspace RPC guest failure: '+String(error.message).slice(0,1000)+'\n');process.exit(1)});
"#;

    struct RpcFixtureBroker(std::process::Child);
    impl Drop for RpcFixtureBroker {
        fn drop(&mut self) {
            self.0.stdin.take();
            let deadline = Instant::now() + Duration::from_secs(3);
            while self.0.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            if self.0.try_wait().ok().flatten().is_none() {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
    }
    fn rpc_file_bytes(store: &LocalStore, root: &ObjectRef, name: &[u8]) -> Vec<u8> {
        let rudder_engine::directory::EntryNode::File {
            object,
            logical_size,
            ..
        } = rudder_engine::tree::lookup(store, root, name)
            .unwrap()
            .unwrap()
            .node
        else {
            panic!("not a regular file")
        };
        assert!(logical_size < 256 * 1024);
        rudder_engine::content::read_file_range(store, &object, 0, logical_size as usize).unwrap()
    }
    fn qualify_workspace_rpc(native: &str, root: &Path) {
        use rudder_engine::{
            directory::{
                DirectoryEntry as PagedEntry, EntryNode as PagedNode, LocalDirectoryStore,
                PagedDirectory,
            },
            execution::{workspace_scope, WorkspaceObservationProfile},
            protocol::canonical_json,
            vfs::{
                rpc::{AttemptContext, ObservationBinding, ObservationClient, Operation},
                NodeKind, ObservationBudget,
            },
            workspace::WorkspaceProfile,
        };
        use std::io::{Read, Write};
        assert_eq!(unsafe { libc::getuid() }, 1500);
        assert_eq!(unsafe { libc::getgid() }, 1500);
        let parent_namespace = fs::read_link("/proc/self/ns/mnt").unwrap();
        private(root);
        let store = LocalStore::open(root.join("cas")).unwrap();
        let manager = VfsManager::open(root.join("vfs"), store.clone()).unwrap();
        let lease = store.begin_staging(Duration::from_secs(600)).unwrap();
        let script = store.put_blob(WORKSPACE_RPC_GUEST.as_bytes()).unwrap();
        let base = PagedDirectory::new(&LocalDirectoryStore::new(&store, &lease.id))
            .build(
                0o755,
                [PagedEntry::new(
                    b"rpc-task.js",
                    PagedNode::File {
                        object: script.clone(),
                        logical_size: script.size_bytes,
                        mode: 0o644,
                    },
                )
                .unwrap()],
            )
            .unwrap();
        store.publish(&lease.id, "rpc-fixture-base", &base).unwrap();
        manager.prepare_paged(&base).unwrap();
        let profile = WorkspaceProfile {
            prepared: vec![vec![b"node_modules".to_vec()]],
            ..WorkspaceProfile::default()
        };
        let profile_ref = store.put_blob(&canonical_json(&profile).unwrap()).unwrap();
        store.pin("rpc-fixture-profile", &profile_ref).unwrap();
        let authority = root.join("authority");
        private(&authority);
        let journal_path = authority.join("journal.sqlite");
        let mut journal = Journal::open(&journal_path).unwrap();
        let fence = journal
            .create_project("rpc-project", "rpc-owner", "baseline", 0)
            .unwrap();
        let owner = OwnerEpoch {
            owner: fence.owner_id.clone(),
            epoch: fence.epoch,
        };
        let mut records = Vec::new();
        for mode in ["live", "revoke", "epoch", "wrong-scope"] {
            let view_id = Uuid::new_v4().to_string();
            let view = manager.fork_paged(&view_id, &base, owner.clone()).unwrap();
            let baseline = view
                .create(1, b"baseline", NodeKind::File, 0o600, b"")
                .unwrap();
            view.write(baseline.ino, 0, b"outside RPC", false).unwrap();
            let foreign_nonce = Uuid::new_v4().to_string();
            view.observe_source(
                &profile.source_exclusions(),
                &foreign_nonce,
                &ObservationBudget::default(),
            )
            .unwrap();
            let cursor = view.dirty_cursor().unwrap();
            let binding = ObservationBinding {
                format_version: 1,
                attempt: AttemptContext {
                    execution_id: Uuid::new_v4().to_string(),
                    operation_id: Uuid::new_v4().to_string(),
                },
                view_id: view_id.clone(),
                owner: owner.clone(),
                namespace: cursor.namespace,
                profile: profile_ref.clone(),
                exclusion_policy_digest: rudder_engine::vfs::exclusion_policy_digest(
                    &profile.source_exclusions(),
                )
                .unwrap(),
            };
            drop(view);
            let run_id = format!("rpc-{mode}");
            let epoch = journal
                .observe_run_attempt(&fence, &run_id, &binding.attempt.execution_id, 0)
                .unwrap();
            let scope = if mode == "wrong-scope" {
                format!("workspace:{}", "0".repeat(64))
            } else {
                workspace_scope(&binding).unwrap()
            };
            let grant = CapabilityGrant {
                principal: format!("agent:{run_id}"),
                run_id: run_id.clone(),
                run_epoch: epoch,
                operations: BTreeSet::from(["workspace:observe".into()]),
                recipients: BTreeMap::new(),
                scopes: BTreeSet::from([scope]),
                topics: BTreeSet::new(),
                expires_at_ms: None,
            };
            let token = journal.issue_capability(&fence, &grant, 0).unwrap();
            let projection = root.join(format!("agent-{mode}"));
            private(&projection);
            let identity = AgentIdentityFile {
                format_version: 1,
                project_id: "rpc-project".into(),
                run_id: run_id.clone(),
                run_epoch: epoch,
                owner_epoch: owner.epoch,
                token: token.clone(),
            };
            fs::write(
                projection.join("identity.json"),
                canonical_json(&identity).unwrap(),
            )
            .unwrap();
            fs::set_permissions(
                projection.join("identity.json"),
                fs::Permissions::from_mode(0o600),
            )
            .unwrap();
            let endpoint = root.join(format!("ws-{mode}"));
            let spec = ExecutionSpec {
                profile: None,
                format_version: 2,
                execution_id: binding.attempt.execution_id.clone(),
                operation_id: binding.attempt.operation_id.clone(),
                execution_root: root.join("executions"),
                manager_root: manager.root().into(),
                store_root: store.root().into(),
                authority_roots: vec![authority.clone()],
                view_id: view_id.clone(),
                owner: owner.clone(),
                command: vec!["/usr/bin/node".into(), "/workspace/rpc-task.js".into()],
                environment: BTreeMap::from([
                    ("RPC_CASE".into(), mode.into()),
                    (
                        "RPC_GIT_REQUIRED".into(),
                        std::env::var("RUDDER_EXECUTION_GIT_QUALIFY")
                            .unwrap_or_else(|_| "0".into()),
                    ),
                    ("FOREIGN_NONCE".into(), foreign_nonce.clone()),
                    (
                        "FORBIDDEN_PATHS".into(),
                        serde_json::to_string(&[
                            journal_path.clone(),
                            store.object_path(&base).unwrap(),
                            manager
                                .root()
                                .join("views")
                                .join(&view_id)
                                .join("namespace.sqlite"),
                        ])
                        .unwrap(),
                    ),
                ]),
                runtime: vec![
                    ReadOnlyRuntime {
                        source: "/usr".into(),
                        guest_path: "/usr".into(),
                    },
                    ReadOnlyRuntime {
                        source: PathBuf::from(native),
                        guest_path: "/opt/runtime/rudder-native".into(),
                    },
                ],
                home_files: vec![],
                broker_directory: Some(projection.clone()),
                broker_binding: Some(BrokerBinding {
                    project_id: identity.project_id.clone(),
                    run_id: run_id.clone(),
                    run_epoch: epoch,
                    owner_epoch: owner.epoch,
                }),
                workspace_observation: Some(WorkspaceObservationProfile {
                    binding: binding.clone(),
                    endpoint_directory: endpoint.clone(),
                    application: None,
                }),
                timeout_seconds: 30,
            };
            let mut broker = RpcFixtureBroker(
                Command::new(native)
                    .args([
                        "fabric",
                        "broker",
                        "--journal",
                        journal_path.to_str().unwrap(),
                        "--identity-file",
                        projection.join("identity.json").to_str().unwrap(),
                        "--socket",
                        projection.join("broker.sock").to_str().unwrap(),
                    ])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap(),
            );
            let ready = Instant::now() + Duration::from_secs(5);
            while !projection.join("broker.sock").exists() {
                assert!(Instant::now() < ready, "scoped broker readiness deadline");
                if broker.0.try_wait().unwrap().is_some() {
                    let mut error = String::new();
                    broker
                        .0
                        .stderr
                        .take()
                        .unwrap()
                        .take(2048)
                        .read_to_string(&mut error)
                        .unwrap();
                    panic!("broker startup failed: {error}")
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            let handoff = ExecutionHandoff::new(&spec).unwrap();
            let mut pane = TerminalPane::spawn_shell_or_command(
                Some(TerminalCommand::with_args(native, handoff.helper_args())),
                TerminalPaneOptions::default(),
            )
            .unwrap();
            let pid = pane.child_process_id().unwrap();
            let handoff = handoff.activate(pid).unwrap();
            let deadline = Instant::now() + Duration::from_secs(45);
            let mut bytes = Vec::new();
            let mut revoked_at = None;
            let mut stale_denied = false;
            let mut pending = None;
            let mut witnesses = Vec::new();
            while pane.is_alive() {
                bytes.extend(pane.drain_output());
                if revoked_at.is_none()
                    && ["revoke", "epoch"].contains(&mode)
                    && String::from_utf8_lossy(&bytes).contains("workspace-rpc-ready")
                {
                    witnesses = pidfds(descendants(pid));
                    let mut stalled =
                        std::os::unix::net::UnixStream::connect(endpoint.join("workspace.sock"))
                            .unwrap();
                    stalled.write_all(&[0, 0]).unwrap();
                    pending = Some(stalled);
                    if mode == "revoke" {
                        journal.revoke_capability(&fence, &token, 0).unwrap();
                    } else {
                        assert_eq!(
                            journal
                                .observe_run_attempt(
                                    &fence,
                                    &run_id,
                                    &Uuid::new_v4().to_string(),
                                    0
                                )
                                .unwrap(),
                            epoch + 1
                        );
                    }
                    revoked_at = Some(Instant::now());
                    let client = ObservationClient::new(
                        endpoint.join("workspace.sock"),
                        endpoint.join("identity.json"),
                        binding.clone(),
                    )
                    .unwrap();
                    stale_denied = client.call(Operation::Cursor).is_err();
                }
                if Instant::now() >= deadline {
                    pane.request_termination();
                    panic!(
                        "workspace helper deadline mode={mode}, output={}",
                        String::from_utf8_lossy(&bytes)
                            .chars()
                            .take(2000)
                            .collect::<String>()
                    );
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            let final_output = Instant::now() + Duration::from_secs(1);
            while pane.try_wait_after_output().unwrap().is_none() && Instant::now() < final_output {
                bytes.extend(pane.drain_output());
                std::thread::sleep(Duration::from_millis(5));
            }
            bytes.extend(pane.drain_output());
            handoff.wait(Duration::from_secs(2)).unwrap();
            let text = String::from_utf8_lossy(&bytes);
            assert!(!text.contains(&token), "credential entered PTY output");
            let path = spec
                .execution_root
                .join("runs")
                .join(&spec.execution_id)
                .join("record.json");
            let record: Option<serde_json::Value> = fs::read(path).ok().map(|bytes| {
                serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["record"].clone()
            });
            let mut report = serde_json::json!({"mode":mode,"phase":record.as_ref().map(|r|r["phase"].clone()),"execution_id":spec.execution_id,"view_id":view_id,"namespace":binding.namespace});
            if mode == "live" {
                let record = record.as_ref().unwrap();
                assert_eq!(record["phase"], "checkpointed", "{text}");
                assert_eq!(record["agent_exit"], 0, "{text}");
                assert!(text.contains("workspace-rpc-live-complete Ω😀"), "{text}");
                let snapshot: VfsCheckpoint = match serde_json::from_str::<rudder_engine::execution::ExecutionRecord>(&record.to_string()).unwrap() {
                    rudder_engine::execution::ExecutionRecord::V1(record) => record.checkpoint.expect("legacy qualifier requires complete local checkpoint"),
                    rudder_engine::execution::ExecutionRecord::V2(_) => panic!("legacy qualifier cannot consume Record2 or partial checkpoint"),
                };
                let proof: serde_json::Value = serde_json::from_slice(&rpc_file_bytes(
                    &store,
                    &snapshot.directory,
                    b"rpc-proof.json",
                ))
                .unwrap();
                for (field, expected) in [
                    ("first", b"first".as_slice()),
                    ("second", b"later".as_slice()),
                ] {
                    let observation: rudder_engine::vfs::VfsObservation =
                        serde_json::from_value(proof[field].clone()).unwrap();
                    assert_eq!(
                        rpc_file_bytes(&store, &observation.directory, b"live.txt"),
                        expected
                    );
                    if field == "second" {
                        assert!(rudder_engine::tree::lookup(
                            &store,
                            &observation.directory,
                            b"node_modules"
                        )
                        .unwrap()
                        .is_none());
                        assert_eq!(
                            store.retention().unwrap().pins.get(&observation.pin_id),
                            Some(&observation.directory)
                        );
                    } else {
                        assert!(!store
                            .retention()
                            .unwrap()
                            .pins
                            .contains_key(&observation.pin_id));
                    }
                }
                assert!(
                    !endpoint.join("workspace.sock").exists(),
                    "checkpoint preceded RPC shutdown"
                );
                report["guest_proof"] = proof;
                report["kernel_namespace_drained_before_checkpoint"] = true.into();
                report["rpc_settled_before_unmount"] = true.into();
            } else if mode == "wrong-scope" {
                assert!(record.as_ref().is_none_or(|r| r["phase"] != "checkpointed"));
                assert!(!text.contains("workspace-rpc-ready"));
                assert!(!endpoint.join("workspace.sock").exists());
                let view = manager.open_view(&view_id, &owner).unwrap();
                assert!(view.lookup(1, b"live.txt").is_err());
                report["denied_before_guest_effects"] = true.into();
            } else {
                assert!(stale_denied, "stale scope retained authority");
                assert!(
                    record
                        .as_ref()
                        .is_some_and(|r| r["phase"] == "orphaned" && r["checkpoint"].is_null()),
                    "{text}"
                );
                let end = Instant::now() + Duration::from_secs(5);
                while !witnesses.iter().all(exited) {
                    assert!(
                        Instant::now() < end,
                        "revocation left witnessed process alive"
                    );
                    std::thread::sleep(Duration::from_millis(5));
                }
                assert!(witnesses.len() >= 3);
                let elapsed = revoked_at.unwrap().elapsed();
                assert!(
                    elapsed < Duration::from_secs(6),
                    "revocation exceeded qualified bound: {elapsed:?}"
                );
                if let Some(mut socket) = pending {
                    socket
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    let mut b = [0];
                    let closed = match socket.read(&mut b) {
                        Ok(0) => true,
                        Err(error) => matches!(
                            error.kind(),
                            std::io::ErrorKind::ConnectionReset
                                | std::io::ErrorKind::ConnectionAborted
                                | std::io::ErrorKind::BrokenPipe
                                | std::io::ErrorKind::NotConnected
                        ),
                        _ => false,
                    };
                    assert!(closed, "stalled RPC survived helper shutdown");
                }
                report["revocation_ms"] = (elapsed.as_millis() as u64).into();
                report["stale_rpc_denied"] = true.into();
                report["witnessed_processes_exited"] = witnesses.len().into();
                report["no_checkpoint_on_authority_loss"] = true.into();
            }
            records.push(report);
            drop(broker);
        }
        assert_eq!(
            fs::read_link("/proc/self/ns/mnt").unwrap(),
            parent_namespace,
            "execution helper changed its controller's mount namespace"
        );
        println!(
            "{}",
            serde_json::json!({"kind":"workspace-observation-rpc","format_version":1,"uid":unsafe{libc::getuid()},"gid":unsafe{libc::getgid()},"parent_mount_namespace_unchanged":true,"records":records,"journal_mounted":false,"cas_mounted":false,"adoption_witness":"unsupported; no peer fsync assertion accepted","default_profile_changed":false})
        );
    }
}
