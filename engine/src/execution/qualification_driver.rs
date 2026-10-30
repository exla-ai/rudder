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

fn main() {
    let args = std::env::args().collect::<Vec<_>>();
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
            let snapshot: VfsCheckpoint =
                serde_json::from_value(record["checkpoint"].clone()).unwrap();
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
    hex::encode(Sha256::digest(bytes))
}
