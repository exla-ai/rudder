use rudder_engine::{
    protocol::{ObjectKind, ObjectRef},
    remote_cache::{CacheError, CacheLimits, CacheScope, RemoteCache},
    remote_source::{
        NodeRemoteSource, NodeSourceConfig, SourceError, SourceRequestControl, TransportProfile,
    },
};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use tempfile::TempDir;
use uuid::Uuid;

// Persistent helper tests need the product's Node22+ runtime. Dependencies are
// installed before the repository test matrix; there is no silent fake-source
// fallback when the executable/TS bundler is absent.
fn serial() -> std::sync::MutexGuard<'static, ()> {
    static SERIAL: Mutex<()> = Mutex::new(());
    SERIAL.lock().unwrap_or_else(|error| error.into_inner())
}
fn temporary() -> TempDir {
    tempfile::tempdir().unwrap()
}
fn until(mut ready: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(5);
    while !ready() {
        assert!(Instant::now() < end, "bounded fixture deadline");
        std::thread::sleep(Duration::from_millis(5));
    }
}
struct Fixture {
    _dir: TempDir,
    node: PathBuf,
    script: PathBuf,
}
impl Fixture {
    fn new(mode: Option<&str>) -> Self {
        let dir = temporary();
        let node = Command::new("node")
            .args(["-p", "process.execPath"])
            .output()
            .expect("Node22+ must be installed for Cloud source integration tests");
        assert!(node.status.success());
        let node = PathBuf::from(String::from_utf8(node.stdout).unwrap().trim());
        let script = dir.path().join("helper.mjs");
        if let Some(mode) = mode {
            let source = r#"
import fs from 'node:fs';
const mode = MODE;
let buffer=Buffer.alloc(0), init;
const frame=(type,id,body=Buffer.alloc(0))=>{const h=Buffer.alloc(16);h[0]=type;h.writeBigUInt64BE(id,4);h.writeUInt32BE(body.length,12);return Buffer.concat([h,body]);};
const json=(type,id,value)=>process.stdout.write(frame(type,id,Buffer.from(JSON.stringify(value))));
process.stdin.on('data',chunk=>{
 buffer=Buffer.concat([buffer,chunk]);
 while(buffer.length>=16&&buffer.length>=16+buffer.readUInt32BE(12)){
  const type=buffer[0],id=buffer.readBigUInt64BE(4),size=buffer.readUInt32BE(12),body=buffer.subarray(16,16+size);buffer=buffer.subarray(16+size);
  if(!init){init=JSON.parse(body);if(mode==='no-hello')continue;
   if(process.argv.some(a=>a.includes('secret-fixture-token'))||Object.values(process.env).some(v=>v.includes('secret-fixture-token')))process.exit(92);
   json(10,0n,{format_version:1,nonce:mode==='wrong-hello'?'wrong':init.nonce,scope:init.scope,max_jobs:init.max_jobs,max_inflight_bytes:init.max_inflight_bytes});continue;}
  if(type===5){process.exit(0);}
  if(type===4)continue;
  if(mode==='exit')process.exit(0);
  if(mode==='hang')continue;
  if(mode==='stderr'){process.stderr.write('secret-fixture-token server-private-text');json(14,id,{code:'failed'});continue;}
  if(mode==='stderr-flood'){process.stderr.write(Buffer.alloc(70000,65));continue;}
  if(mode==='oversized'){const h=Buffer.alloc(16);h[0]=11;h.writeBigUInt64BE(id,4);h.writeUInt32BE(65537,12);process.stdout.write(h);continue;}
  if(mode==='partial'){const h=frame(13,id,Buffer.alloc(10));process.stdout.write(h.subarray(0,18),()=>process.exit(0));continue;}
  if(mode==='foreign'){json(14,id+1n,{code:'failed'});continue;}
  if(mode==='wrong-hash'){process.stdout.write(frame(11,id,Buffer.from('wrong')));process.stdout.write(frame(12,id));continue;}
  if(mode==='duplicate'){json(14,id,{code:'failed'});json(14,id,{code:'failed'});continue;}
 }
});
"#;
            fs::write(
                &script,
                source.replace("MODE", &serde_json::to_string(mode).unwrap()),
            )
            .unwrap();
        } else {
            let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
            let module = dir.path().join("fabric-fetch.mjs");
            let output = Command::new(root.join("node_modules/.bin/esbuild"))
                .current_dir(root)
                .arg("src/fabric-fetch.ts")
                .args(["--bundle", "--platform=node", "--format=esm"])
                .arg(format!("--outfile={}", module.display()))
                .output()
                .expect("npm ci must precede Cloud source integration tests");
            assert!(output.status.success(), "helper TS fixture bundle failed");
            fs::write(
                &script,
                format!(
                    "import {{runFabricFetchHelper}} from {}; await runFabricFetchHelper();",
                    serde_json::to_string(&format!("file://{}", module.display())).unwrap()
                ),
            )
            .unwrap();
        }
        Self {
            _dir: dir,
            node,
            script,
        }
    }
    fn config(&self, base: &str) -> NodeSourceConfig {
        let mut result = NodeSourceConfig::new(
            CacheScope {
                provider_id: "http-fixture".into(),
                tenant_id: Uuid::new_v4().to_string(),
            },
            base.into(),
            BTreeMap::from([("Authorization".into(), "Bearer secret-fixture-token".into())]),
        );
        result.transport = TransportProfile::LoopbackHttpTest;
        result.max_jobs = 2;
        result.http_timeout = Duration::from_secs(2);
        result.handshake_timeout = Duration::from_secs(3);
        result
    }
    fn spawn(&self, base: &str) -> NodeRemoteSource {
        NodeRemoteSource::spawn(&self.node, &self.script, self.config(base)).unwrap()
    }
}
struct Http {
    url: String,
    calls: Arc<AtomicUsize>,
    closed: Arc<AtomicUsize>,
    auth_ok: Arc<AtomicBool>,
    mode: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    workers: Vec<std::thread::JoinHandle<()>>,
}
impl Http {
    fn new(body: Vec<u8>, root: serde_json::Value) -> Self {
        Self::with_inherited_nonblocking(body, root, false)
    }
    fn with_inherited_nonblocking(
        body: Vec<u8>,
        root: serde_json::Value,
        force_inherited_nonblocking: bool,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let closed = Arc::new(AtomicUsize::new(0));
        let auth_ok = Arc::new(AtomicBool::new(true));
        let mode = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let body = Arc::new(body);
        let root = Arc::new(serde_json::to_vec(&root).unwrap());
        let mut workers = Vec::new();
        for _ in 0..4 {
            let listener = listener.try_clone().unwrap();
            let calls = calls.clone();
            let closed = closed.clone();
            let auth_ok = auth_ok.clone();
            let mode = mode.clone();
            let stop = stop.clone();
            let body = body.clone();
            let root = root.clone();
            workers.push(std::thread::spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            if force_inherited_nonblocking {
                                // Model Darwin on Linux too, with enough
                                // backpressure to exercise a partial write.
                                stream.set_nonblocking(true).unwrap();
                                let size: libc::c_int = 64 * 1024;
                                let result = unsafe {
                                    libc::setsockopt(
                                        stream.as_raw_fd(),
                                        libc::SOL_SOCKET,
                                        libc::SO_SNDBUF,
                                        &size as *const _ as *const libc::c_void,
                                        std::mem::size_of_val(&size) as libc::socklen_t,
                                    )
                                };
                                assert_eq!(result, 0);
                            }
                            // BSD accept inherits O_NONBLOCK. This worker uses
                            // blocking I/O with finite socket timeouts instead.
                            stream.set_nonblocking(false).unwrap();
                            stream.set_read_timeout(Some(Duration::from_millis(50))).unwrap(); stream.set_write_timeout(Some(Duration::from_secs(2))).unwrap();
                            let mut request = Vec::new(); let mut part = [0; 1024];
                            while request.len() <= 16384 && !request.windows(4).any(|v| v == b"\r\n\r\n") && !stop.load(Ordering::Acquire) {
                                match stream.read(&mut part) { Ok(0) => break, Ok(n) => request.extend_from_slice(&part[..n]), Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}, Err(_) => break }
                            }
                            if !request.windows(4).any(|v| v == b"\r\n\r\n") { continue; }
                            let request = String::from_utf8_lossy(&request).to_ascii_lowercase();
                            if !request.contains("authorization: bearer secret-fixture-token\r\n") { auth_ok.store(false, Ordering::Release); }
                            calls.fetch_add(1, Ordering::AcqRel);
                            if mode.load(Ordering::Acquire) == 2 {
                                while !stop.load(Ordering::Acquire) {
                                    match stream.read(&mut part) { Ok(0) | Err(_) if stop.load(Ordering::Acquire) => break, Ok(0) => { closed.fetch_add(1, Ordering::AcqRel); break; }, Ok(_) => {}, Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}, Err(_) => break }
                                }
                            } else {
                                let current = mode.load(Ordering::Acquire); let is_root = request.lines().next().unwrap_or("").contains("/roots/");
                                let bytes = if current == 3 { b"private provider text".as_slice() } else if is_root { root.as_slice() } else { body.as_slice() };
                                let mut corrupted = Vec::new(); let bytes = if current == 1 && !bytes.is_empty() { corrupted.extend_from_slice(bytes); corrupted[0] ^= 1; corrupted.as_slice() } else { bytes };
                                let header = format!("HTTP/1.1 {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", if current == 3 { "401 Unauthorized" } else { "200 OK" }, bytes.len());
                                let _ = stream.write_all(header.as_bytes()); let _ = stream.write_all(bytes);
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(2)),
                        Err(_) => return,
                    }
                }
            }));
        }
        Self {
            url: format!(
                "http://{}/api/rudder/fabric",
                listener.local_addr().unwrap()
            ),
            calls,
            closed,
            auth_ok,
            mode,
            stop,
            workers,
        }
    }
}
impl Drop for Http {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        for worker in self.workers.drain(..) {
            worker.join().unwrap();
        }
    }
}
fn root(reference: &ObjectRef) -> serde_json::Value {
    serde_json::json!({"format_version":1,"root_id":"prepared","root":reference,"durability":"replicated","published_at_ms":1,"object_count":1,"total_bytes":reference.size_bytes})
}
fn cache(base: &Path, source: &NodeRemoteSource) -> RemoteCache {
    RemoteCache::open(
        base,
        Arc::new(source.clone()),
        CacheLimits {
            fetch_timeout: Duration::from_secs(2),
            ..Default::default()
        },
    )
    .unwrap()
}
fn stop(source: &NodeRemoteSource) {
    source.terminate();
    until(|| source.stats().unwrap().child_exited);
}

#[test]
fn inherited_nonblocking_http_stream_delivers_complete_4mib_body_under_backpressure() {
    let body = vec![0x39; 4 * 1024 * 1024];
    let reference = ObjectRef::from_bytes(ObjectKind::Blob, &body);
    let http = Http::with_inherited_nonblocking(body.clone(), root(&reference), true);
    let address = http
        .url
        .strip_prefix("http://")
        .unwrap()
        .split('/')
        .next()
        .unwrap();
    let mut client = TcpStream::connect(address).unwrap();
    client.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    client.set_write_timeout(Some(Duration::from_secs(2))).unwrap();
    let request = format!(
        "GET /api/rudder/fabric/objects/blob/{}/{} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer secret-fixture-token\r\n\r\n",
        reference.digest, reference.size_bytes,
    );
    client.write_all(request.as_bytes()).unwrap();
    until(|| http.calls.load(Ordering::Acquire) == 1);
    // Hold the reader long enough for a nonblocking writer to exhaust its
    // small send buffer. A bounded blocking writer waits for us to drain it.
    std::thread::sleep(Duration::from_millis(50));
    let mut response = Vec::new();
    client.read_to_end(&mut response).unwrap();
    let header_end = response
        .windows(4)
        .position(|part| part == b"\r\n\r\n")
        .unwrap()
        + 4;
    assert!(response.starts_with(b"HTTP/1.1 200 OK\r\n"));
    assert_eq!(response.len() - header_end, body.len());
    assert_eq!(ObjectRef::from_bytes(ObjectKind::Blob, &response[header_end..]), reference);
    assert!(http.auth_ok.load(Ordering::Acquire));
}

#[test]
fn actual_persistent_source_fetches_4mib_and_receipt_then_cache_reads_offline() {
    let _serial = serial();
    let fixture = Fixture::new(None);
    let body = vec![0x6d; 4 * 1024 * 1024];
    let reference = ObjectRef::from_bytes(ObjectKind::Blob, &body);
    let http = Http::new(body.clone(), root(&reference));
    let source = fixture.spawn(&http.url);
    let base = temporary();
    let cache = cache(base.path(), &source);
    let result = cache
        .request(&reference, Instant::now() + Duration::from_secs(5))
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(result.bytes(), body);
    drop(result);
    let receipt = source
        .get_root(
            "prepared",
            &SourceRequestControl::new(Instant::now() + Duration::from_secs(2)),
        )
        .unwrap();
    assert_eq!(receipt.root, reference);
    assert_eq!(receipt.object_count, 1);
    assert_eq!(http.calls.load(Ordering::Acquire), 2);
    assert!(http.auth_ok.load(Ordering::Acquire));
    stop(&source);
    drop(http);
    let offline = cache
        .request(&reference, Instant::now() + Duration::from_secs(1))
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(offline.bytes(), body);
    assert_eq!(cache.stats().unwrap().remote_fetches, 1);
    assert_eq!(source.stats().unwrap().pending_jobs, 0);
}

#[test]
fn actual_cancel_aborts_http_and_drains_before_job_release() {
    let _serial = serial();
    let fixture = Fixture::new(None);
    let body = b"fixture".to_vec();
    let reference = ObjectRef::from_bytes(ObjectKind::Blob, &body);
    let http = Http::new(body, root(&reference));
    http.mode.store(2, Ordering::Release);
    let source = fixture.spawn(&http.url);
    let control = SourceRequestControl::new(Instant::now() + Duration::from_secs(2));
    let worker_source = source.clone();
    let worker_control = control.clone();
    let worker = std::thread::spawn(move || worker_source.get_root("prepared", &worker_control));
    until(|| http.calls.load(Ordering::Acquire) == 1);
    control.cancel();
    assert_eq!(worker.join().unwrap().unwrap_err(), SourceError::Cancelled);
    until(|| http.closed.load(Ordering::Acquire) == 1);
    assert_eq!(source.stats().unwrap().pending_jobs, 0);
    stop(&source);
}

#[test]
fn concurrent_native_callers_keep_request_ids_ordered_on_one_helper() {
    let _serial = serial();
    let fixture = Fixture::new(None);
    let reference = ObjectRef::from_bytes(ObjectKind::Blob, b"one");
    let http = Http::new(b"one".to_vec(), root(&reference));
    let mut config = fixture.config(&http.url);
    config.max_jobs = 8;
    let source = NodeRemoteSource::spawn(&fixture.node, &fixture.script, config).unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(8));
    let workers: Vec<_> = (0..8)
        .map(|_| {
            let source = source.clone();
            let barrier = barrier.clone();
            let reference = reference.clone();
            std::thread::spawn(move || {
                for _ in 0..8 {
                    barrier.wait();
                    let receipt = source
                        .get_root(
                            "prepared",
                            &SourceRequestControl::new(Instant::now() + Duration::from_secs(3)),
                        )
                        .unwrap();
                    assert_eq!(receipt.root, reference);
                }
            })
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }
    assert_eq!(http.calls.load(Ordering::Acquire), 64);
    assert_eq!(source.stats().unwrap().pending_jobs, 0);
    assert!(!source.stats().unwrap().child_exited);
    stop(&source);
}

#[test]
fn helper_survives_the_temporary_thread_that_prepares_it() {
    let _serial = serial();
    let fixture = Fixture::new(None);
    let reference = ObjectRef::from_bytes(ObjectKind::Blob, b"thread ownership");
    let http = Http::new(b"thread ownership".to_vec(), root(&reference));
    let config = fixture.config(&http.url);
    let node = fixture.node.clone();
    let script = fixture.script.clone();
    let source = std::thread::spawn(move || NodeRemoteSource::spawn(node, script, config).unwrap())
        .join()
        .unwrap();
    std::thread::sleep(Duration::from_millis(50));
    let receipt = source
        .get_root(
            "prepared",
            &SourceRequestControl::new(Instant::now() + Duration::from_secs(2)),
        )
        .unwrap();
    assert_eq!(receipt.root, reference);
    assert!(!source.stats().unwrap().child_exited);
    stop(&source);
}

#[test]
fn stalled_http_deadline_and_bad_hash_never_admit_bytes() {
    let _serial = serial();
    let fixture = Fixture::new(None);
    let body = b"right".to_vec();
    let reference = ObjectRef::from_bytes(ObjectKind::Blob, &body);
    let http = Http::new(body, root(&reference));
    let source = fixture.spawn(&http.url);
    http.mode.store(2, Ordering::Release);
    let start = Instant::now();
    let failure = source
        .get_root(
            "prepared",
            &SourceRequestControl::new(start + Duration::from_millis(50)),
        )
        .unwrap_err();
    assert_eq!(failure, SourceError::TimedOut);
    assert!(start.elapsed() < Duration::from_secs(2));
    http.mode.store(1, Ordering::Release);
    let base = temporary();
    let cache = cache(base.path(), &source);
    assert!(cache
        .request(&reference, Instant::now() + Duration::from_secs(2))
        .unwrap()
        .wait()
        .is_err());
    assert_eq!(cache.stats().unwrap().disk_entries, 0);
    stop(&source);
}

#[test]
fn hung_child_retains_native_and_cache_admission_until_actual_reap() {
    let _serial = serial();
    let fixture = Fixture::new(Some("hang"));
    let mut config = fixture.config("http://127.0.0.1:1/api/rudder/fabric");
    config.max_jobs = 1;
    let scope = config.scope.clone();
    let source = NodeRemoteSource::spawn(&fixture.node, &fixture.script, config).unwrap();
    let base = temporary();
    let cache = cache(base.path(), &source);
    let reference = ObjectRef::from_bytes(ObjectKind::Blob, b"hung");
    let ticket = cache
        .request(&reference, Instant::now() + Duration::from_millis(60))
        .unwrap();
    assert_eq!(ticket.wait().unwrap_err(), CacheError::TimedOut);
    until(|| source.stats().unwrap().pending_jobs == 1);
    std::thread::sleep(Duration::from_millis(60));
    assert_eq!(cache.stats().unwrap().inflight_jobs, 1);
    assert_eq!(source.stats().unwrap().pending_bytes, 4);
    assert_eq!(
        source
            .get_root(
                "prepared",
                &SourceRequestControl::new(Instant::now() + Duration::from_secs(1))
            )
            .unwrap_err(),
        SourceError::Busy
    );
    let mut duplicate = fixture.config("http://127.0.0.1:1/api/rudder/fabric");
    duplicate.scope = scope;
    assert_eq!(
        NodeRemoteSource::spawn(&fixture.node, &fixture.script, duplicate).unwrap_err(),
        SourceError::Busy
    );
    stop(&source);
    until(|| cache.stats().unwrap().inflight_jobs == 0);
    assert_eq!(source.stats().unwrap().pending_jobs, 0);
}

#[test]
fn source_saturation_preserves_eagain_through_the_cache_boundary() {
    let _serial = serial();
    let fixture = Fixture::new(Some("hang"));
    let mut config = fixture.config("http://127.0.0.1:1/api/rudder/fabric");
    config.max_jobs = 1;
    let source = NodeRemoteSource::spawn(&fixture.node, &fixture.script, config).unwrap();
    let worker_source = source.clone();
    let worker = std::thread::spawn(move || {
        worker_source.get_root(
            "prepared",
            &SourceRequestControl::new(Instant::now() + Duration::from_secs(2)),
        )
    });
    until(|| source.stats().unwrap().pending_jobs == 1);
    let base = temporary();
    let cache = cache(base.path(), &source);
    let reference = ObjectRef::from_bytes(ObjectKind::Blob, b"busy");
    let error = cache
        .request(&reference, Instant::now() + Duration::from_secs(1))
        .unwrap()
        .wait()
        .unwrap_err();
    assert_eq!(error, CacheError::Busy);
    assert_eq!(error.errno(), libc::EAGAIN);
    assert_eq!(cache.stats().unwrap().disk_entries, 0);
    stop(&source);
    assert!(worker.join().unwrap().is_err());
}

#[test]
fn foreign_partial_oversized_duplicate_and_exit_fail_closed_and_reap() {
    let _serial = serial();
    for mode in [
        "foreign",
        "partial",
        "oversized",
        "duplicate",
        "exit",
        "stderr-flood",
    ] {
        let fixture = Fixture::new(Some(mode));
        let source = fixture.spawn("http://127.0.0.1:1/api/rudder/fabric");
        assert!(
            source
                .get_root(
                    "prepared",
                    &SourceRequestControl::new(Instant::now() + Duration::from_secs(2))
                )
                .is_err(),
            "{mode}"
        );
        until(|| source.stats().unwrap().child_exited);
        assert_eq!(source.stats().unwrap().pending_jobs, 0);
    }
}

#[test]
fn wrong_hash_helper_never_writes_unverified_cache_bytes_and_stderr_is_suppressed() {
    let _serial = serial();
    let fixture = Fixture::new(Some("wrong-hash"));
    let source = fixture.spawn("http://127.0.0.1:1/api/rudder/fabric");
    let base = temporary();
    let cache = cache(base.path(), &source);
    let reference = ObjectRef::from_bytes(ObjectKind::Blob, b"right");
    assert!(cache
        .request(&reference, Instant::now() + Duration::from_secs(2))
        .unwrap()
        .wait()
        .is_err());
    assert_eq!(cache.stats().unwrap().disk_entries, 0);
    until(|| source.stats().unwrap().child_exited);
    let fixture = Fixture::new(Some("stderr"));
    let source = fixture.spawn("http://127.0.0.1:1/api/rudder/fabric");
    let error = source
        .get_root(
            "prepared",
            &SourceRequestControl::new(Instant::now() + Duration::from_secs(2)),
        )
        .unwrap_err();
    assert_eq!(error, SourceError::RemoteFailed);
    assert!(!format!("{error:?} {error} {source:?}").contains("secret-fixture-token"));
    until(|| source.stats().unwrap().stderr_bytes_suppressed > 0);
    stop(&source);
}

#[test]
fn handshake_mismatch_timeout_and_invalid_config_never_start_unbounded_replacements() {
    let _serial = serial();
    for mode in ["wrong-hello", "no-hello"] {
        let fixture = Fixture::new(Some(mode));
        let mut config = fixture.config("http://127.0.0.1:1/api/rudder/fabric");
        config.handshake_timeout = Duration::from_millis(100);
        assert!(NodeRemoteSource::spawn(&fixture.node, &fixture.script, config).is_err());
    }
    let fixture = Fixture::new(Some("hang"));
    for base in [
        "http://example.com/api",
        "http://localhost/api",
        "http://u:p@127.0.0.1/api",
        "http://127.0.0.1/api?q=x",
        "http://127.0.0.1/api#x",
    ] {
        assert_eq!(
            NodeRemoteSource::spawn(&fixture.node, &fixture.script, fixture.config(base))
                .unwrap_err(),
            SourceError::InvalidConfig
        );
    }
    let mut config = fixture.config("http://127.0.0.1/api");
    config.http_timeout = Duration::from_nanos(1);
    assert_eq!(
        NodeRemoteSource::spawn(&fixture.node, &fixture.script, config).unwrap_err(),
        SourceError::InvalidConfig
    );
}

#[test]
fn actual_auth_revocation_is_fail_closed_without_credential_fallback() {
    let _serial = serial();
    let fixture = Fixture::new(None);
    let reference = ObjectRef::from_bytes(ObjectKind::Blob, b"a");
    let http = Http::new(b"a".to_vec(), root(&reference));
    http.mode.store(3, Ordering::Release);
    let source = fixture.spawn(&http.url);
    let error = source
        .get_root(
            "prepared",
            &SourceRequestControl::new(Instant::now() + Duration::from_secs(2)),
        )
        .unwrap_err();
    assert!(!error.to_string().contains("secret"));
    until(|| source.stats().unwrap().child_exited);
    assert_eq!(source.stats().unwrap().pending_jobs, 0);
    assert_eq!(
        source
            .get_root(
                "prepared",
                &SourceRequestControl::new(Instant::now() + Duration::from_secs(1))
            )
            .unwrap_err(),
        SourceError::Closed
    );
    assert_eq!(http.calls.load(Ordering::Acquire), 1);
}
