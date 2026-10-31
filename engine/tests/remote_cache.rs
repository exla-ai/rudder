use rudder_engine::{
    protocol::{canonical_json, ObjectKind, ObjectRef},
    remote_cache::{
        CacheError, CacheLimits, CacheScope, CachedObject, FetchControl, ImmutableObjectSource,
        RemoteCache, TRANSFER_BUFFER_BYTES,
    },
};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Condvar, Mutex,
    },
    time::{Duration, Instant},
};
use uuid::Uuid;

struct Source {
    scope: CacheScope,
    objects: Mutex<BTreeMap<ObjectRef, Vec<u8>>>,
    calls: AtomicUsize,
    active: AtomicUsize,
    peak: AtomicUsize,
    gate: Mutex<bool>,
    changed: Condvar,
    honor_cancel: bool,
    fail: AtomicBool,
    mode: AtomicUsize,
}
impl Source {
    fn new(
        tenant: &str,
        objects: &[(ObjectRef, Vec<u8>)],
        blocked: bool,
        honor_cancel: bool,
    ) -> Arc<Self> {
        Arc::new(Self {
            scope: CacheScope {
                provider_id: "fixture-provider".into(),
                tenant_id: tenant.into(),
            },
            objects: Mutex::new(objects.iter().cloned().collect()),
            calls: AtomicUsize::new(0),
            active: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            gate: Mutex::new(!blocked),
            changed: Condvar::new(),
            honor_cancel,
            fail: AtomicBool::new(false),
            mode: AtomicUsize::new(0),
        })
    }
    fn release(&self) {
        *self.gate.lock().unwrap() = true;
        self.changed.notify_all();
    }
}
impl ImmutableObjectSource for Source {
    fn scope(&self) -> CacheScope {
        self.scope.clone()
    }
    fn fetch(
        &self,
        reference: &ObjectRef,
        control: &FetchControl,
        output: &mut dyn Write,
    ) -> anyhow::Result<()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(active, Ordering::SeqCst);
        struct Active<'a>(&'a AtomicUsize);
        impl Drop for Active<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }
        let _active = Active(&self.active);
        let mut gate = self.gate.lock().unwrap();
        while !*gate {
            if self.honor_cancel {
                control.check()?;
            }
            gate = self
                .changed
                .wait_timeout(gate, Duration::from_millis(5))
                .unwrap()
                .0;
        }
        drop(gate);
        if self.fail.load(Ordering::SeqCst) {
            anyhow::bail!("synthetic-provider-secret-must-not-escape")
        }
        let mut bytes = self
            .objects
            .lock()
            .unwrap()
            .get(reference)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("private-provider-detail"))?;
        match self.mode.load(Ordering::SeqCst) {
            1 => {
                if let Some(first) = bytes.first_mut() {
                    *first ^= 0xff
                }
            }
            2 => bytes.push(0),
            3 => {
                bytes.pop();
            }
            4 => panic!("synthetic source panic"),
            5 => return Err(CacheError::Busy.into()),
            6 => return Err(CacheError::TimedOut.into()),
            7 => return Err(CacheError::Cancelled.into()),
            8 => return Err(CacheError::UnsafePath.into()),
            _ => {}
        }
        for chunk in bytes.chunks(TRANSFER_BUFFER_BYTES) {
            output.write_all(chunk)?;
        }
        Ok(())
    }
}
fn fixture() -> tempfile::TempDir {
    let base = std::env::var_os("RUDDER_TEST_TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            if Path::new("/workspace").is_dir() {
                "/workspace".into()
            } else {
                std::env::temp_dir()
            }
        });
    tempfile::Builder::new()
        .prefix("rudder-remote-cache-")
        .tempdir_in(base)
        .unwrap()
}
fn object(bytes: &[u8]) -> (ObjectRef, Vec<u8>) {
    (
        ObjectRef::from_bytes(ObjectKind::Blob, bytes),
        bytes.to_vec(),
    )
}

#[test]
fn trusted_transport_retry_errors_survive_without_leaking_provider_details() {
    let base = fixture();
    let value = object(b"retryable transport fixture");
    let source = Source::new("retry-errors", &[value.clone()], false, true);
    let cache = open(base.path(), source.clone(), small_limits());
    for (mode, expected) in [
        (5, CacheError::Busy),
        (6, CacheError::TimedOut),
        (7, CacheError::Cancelled),
        (8, CacheError::SourceFailed),
    ] {
        source.mode.store(mode, Ordering::SeqCst);
        assert_eq!(
            cache
                .request(&value.0, deadline())
                .unwrap()
                .wait()
                .unwrap_err(),
            expected
        );
        assert_eq!(cache.stats().unwrap().disk_entries, 0);
    }
    source.mode.store(0, Ordering::SeqCst);
    assert_eq!(
        cache
            .request(&value.0, deadline())
            .unwrap()
            .wait()
            .unwrap()
            .bytes(),
        value.1
    );
}
fn small_limits() -> CacheLimits {
    CacheLimits {
        workers: 2,
        max_jobs: 8,
        max_tickets: 32,
        max_inflight_bytes: 128,
        max_resident_bytes: 256,
        max_borrowed_bytes: 128,
        disk_quota_bytes: 256,
        max_disk_entries: 16,
        max_recovery_entries: 64,
        fetch_timeout: Duration::from_secs(2),
    }
}
fn open(base: &Path, source: Arc<Source>, limits: CacheLimits) -> RemoteCache {
    RemoteCache::open(base.canonicalize().unwrap(), source, limits).unwrap()
}
fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(2)
}
fn until(mut condition: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(3);
    while !condition() {
        assert!(Instant::now() < end, "bounded condition did not resolve");
        std::thread::sleep(Duration::from_millis(2));
    }
}
fn read(cache: &RemoteCache, reference: &ObjectRef) -> CachedObject {
    cache
        .request(reference, deadline())
        .unwrap()
        .wait()
        .unwrap()
}
fn entry(cache: &RemoteCache, reference: &ObjectRef) -> PathBuf {
    fs::read_dir(cache.namespace_root().join("entries"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .contains(&reference.digest)
        })
        .unwrap()
}
fn reopen(base: &Path, source: Arc<Source>, limits: CacheLimits) -> RemoteCache {
    let end = Instant::now() + Duration::from_secs(3);
    loop {
        match RemoteCache::open(base.canonicalize().unwrap(), source.clone(), limits.clone()) {
            Ok(cache) => return cache,
            Err(CacheError::Busy) => {
                assert!(Instant::now() < end, "cache lock did not drain");
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(error) => panic!("unexpected reopen:{error}"),
        }
    }
}
fn drained(root: &Path) {
    use fs2::FileExt;
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(root.join("lock"))
        .unwrap();
    until(|| file.try_lock_exclusive().is_ok());
    FileExt::unlock(&file).unwrap();
}

#[test]
fn same_object_fetches_coalesce_and_local_hits_never_require_network() {
    let base = fixture();
    let (reference, bytes) = object(b"0123456789abcdef");
    let source = Source::new("tenant", &[(reference.clone(), bytes.clone())], true, true);
    let mut limits = small_limits();
    limits.max_jobs = 1;
    limits.max_inflight_bytes = 16;
    let cache = open(base.path(), source.clone(), limits.clone());
    let mut tickets = (0..20)
        .map(|_| cache.request(&reference, deadline()).unwrap())
        .collect::<Vec<_>>();
    until(|| source.calls.load(Ordering::SeqCst) == 1);
    assert!(tickets.iter().all(|ticket| ticket.try_result().is_none()));
    assert_eq!(cache.stats().unwrap().inflight_bytes, 16);
    tickets[0].cancel();
    assert_eq!(tickets[0].wait().unwrap_err(), CacheError::Cancelled);
    source.release();
    let mut results = Vec::new();
    for ticket in tickets.drain(1..) {
        results.push(ticket.wait().unwrap());
    }
    assert!(results.iter().all(|result| result.bytes() == bytes));
    assert_eq!(cache.stats().unwrap().borrowed_bytes, 16);
    assert_eq!(source.calls.load(Ordering::SeqCst), 1);
    let pointer = results[0].bytes().as_ptr();
    assert!(results
        .iter()
        .all(|result| result.bytes().as_ptr() == pointer));
    drop(results);
    drop(tickets);
    until(|| cache.stats().unwrap().resident_bytes == 0);
    source.fail.store(true, Ordering::SeqCst);
    let local = read(&cache, &reference);
    assert_eq!(local.bytes(), bytes);
    assert_eq!(source.calls.load(Ordering::SeqCst), 1);
    assert_eq!(cache.stats().unwrap().verified_disk_hits, 1);
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        fs::metadata(entry(&cache, &reference))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o444
    );
    assert_eq!(
        fs::read_dir(cache.namespace_root().join("tmp"))
            .unwrap()
            .count(),
        0
    );
    drop(local);
    drop(cache);
    let cache = reopen(base.path(), source.clone(), limits);
    assert_eq!(cache.recovery().recovered_entries, 1);
    assert_eq!(read(&cache, &reference).bytes(), bytes);
    assert_eq!(source.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn fixed_workers_jobs_bytes_and_ticket_admission_are_bounded() {
    let base = fixture();
    let objects = (0..9).map(|i| object(&[i; 8])).collect::<Vec<_>>();
    let source = Source::new("bounded", &objects, true, true);
    let mut limits = small_limits();
    limits.max_jobs = 4;
    limits.max_tickets = 4;
    limits.max_inflight_bytes = 32;
    limits.workers = 2;
    let cache = open(base.path(), source.clone(), limits);
    let mut tickets = (0..4)
        .map(|i| cache.request(&objects[i].0, deadline()).unwrap())
        .collect::<Vec<_>>();
    until(|| source.active.load(Ordering::SeqCst) == 2);
    assert_eq!(
        cache.request(&objects[4].0, deadline()).unwrap_err(),
        CacheError::Busy
    );
    let stats = cache.stats().unwrap();
    assert_eq!(stats.inflight_jobs, 4);
    assert_eq!(stats.inflight_bytes, 32);
    assert_eq!(stats.live_tickets, 4);
    assert!(source.peak.load(Ordering::SeqCst) <= 2);
    tickets.pop(); // admission for the queued job itself remains until a worker observes cancellation
    assert_eq!(
        cache.request(&objects[4].0, deadline()).unwrap_err(),
        CacheError::Busy
    );
    source.release();
    for ticket in tickets {
        assert_eq!(ticket.wait().unwrap().bytes().len(), 8);
    }
    until(|| cache.stats().unwrap().inflight_jobs == 0);
    assert!(source.peak.load(Ordering::SeqCst) <= 2);
}

#[test]
fn independent_borrowed_and_resident_quotas_never_invent_space_for_held_snapshots() {
    let base = fixture();
    let objects = vec![
        object(b"aaaaaaaa"),
        object(b"bbbbbbbb"),
        object(b"cccccccc"),
    ];
    let source = Source::new("memory", &objects, false, true);
    let mut limits = small_limits();
    limits.max_resident_bytes = 16;
    limits.max_borrowed_bytes = 8;
    let cache = open(base.path(), source, limits);
    let first = cache.request(&objects[0].0, deadline()).unwrap();
    let first_data = first.wait().unwrap();
    let clone = first_data.clone();
    drop(first_data);
    let second = cache.request(&objects[1].0, deadline()).unwrap();
    until(|| cache.stats().unwrap().inflight_jobs == 0);
    assert_eq!(second.wait().unwrap_err(), CacheError::Busy);
    assert_eq!(cache.stats().unwrap().borrowed_bytes, 8);
    assert_eq!(
        cache.request(&objects[2].0, deadline()).unwrap_err(),
        CacheError::Busy
    );
    drop(clone);
    let second_data = second.wait().unwrap();
    assert_eq!(second_data.bytes(), b"bbbbbbbb");
    assert_eq!(cache.stats().unwrap().resident_bytes, 16);
    drop(first);
    until(|| cache.stats().unwrap().resident_bytes == 8);
    drop(second_data);
    drop(second);
    assert_eq!(read(&cache, &objects[2].0).bytes(), b"cccccccc");
}

#[test]
fn borrowed_entries_prevent_disk_eviction_and_all_busy_is_eagain() {
    use std::future::Future;
    use std::task::{Context, Poll, Wake, Waker};
    // A published result is available before finish_job returns and drops its
    // own Job/result Arc. Pause that exact phase instead of relying on whether
    // the test or worker happens to run first after wait() wakes the caller.
    struct FinishingWorker {
        entered: AtomicBool,
        released: Mutex<bool>,
        changed: Condvar,
    }
    impl Wake for FinishingWorker {
        fn wake(self: Arc<Self>) {
            let mut released = self.released.lock().unwrap();
            self.entered.store(true, Ordering::SeqCst);
            while !*released {
                released = self.changed.wait(released).unwrap();
            }
        }
    }
    struct ResumeWorker(Arc<FinishingWorker>);
    impl Drop for ResumeWorker {
        fn drop(&mut self) {
            *self.0.released.lock().unwrap() = true;
            self.0.changed.notify_all();
        }
    }
    let base = fixture();
    let objects = vec![object(b"aaaaaaaa"), object(b"bbbbbbbb")];
    let source = Source::new("disk", &objects, true, true);
    let mut limits = small_limits();
    limits.disk_quota_bytes = 8;
    limits.max_disk_entries = 1;
    let cache = open(base.path(), source.clone(), limits);
    let finishing = Arc::new(FinishingWorker {
        entered: AtomicBool::new(false),
        released: Mutex::new(false),
        changed: Condvar::new(),
    });
    let resume = ResumeWorker(finishing.clone());
    let waker = Waker::from(finishing.clone());
    let mut first_ticket = cache.request(&objects[0].0, deadline()).unwrap();
    assert!(matches!(
        std::pin::Pin::new(&mut first_ticket).poll(&mut Context::from_waker(&waker)),
        Poll::Pending
    ));
    source.release();
    until(|| finishing.entered.load(Ordering::SeqCst));
    let first = first_ticket.wait().unwrap();
    assert_eq!(source.active.load(Ordering::SeqCst), 0);
    let ticket = cache.request(&objects[1].0, deadline()).unwrap();
    let error = ticket.wait().unwrap_err();
    assert_eq!(error.errno(), libc::EAGAIN);
    assert_eq!(source.calls.load(Ordering::SeqCst), 1);
    assert_eq!(cache.stats().unwrap().disk_bytes, 8);
    drop(ticket);
    drop(first);
    drop(first_ticket);
    assert_eq!(cache.stats().unwrap().borrowed_bytes, 0);
    assert_eq!(cache.stats().unwrap().resident_bytes, 8);
    assert_eq!(cache.stats().unwrap().inflight_jobs, 0);
    // The transport has settled, but the paused worker still owns the verified
    // result. EAGAIN remains correct until that actual owner releases it.
    let still_finishing = cache.request(&objects[1].0, deadline()).unwrap();
    assert_eq!(still_finishing.wait().unwrap_err(), CacheError::Busy);
    drop(still_finishing);
    assert_eq!(source.calls.load(Ordering::SeqCst), 1);
    drop(resume);
    // This is the eviction predicate's actual last-Arc settlement, not a delay
    // or a retry that masks an unexpected request failure.
    until(|| cache.stats().unwrap().resident_bytes == 0);
    assert_eq!(read(&cache, &objects[1].0).bytes(), b"bbbbbbbb");
    let stats = cache.stats().unwrap();
    assert_eq!(stats.evictions, 1);
    assert_eq!(stats.disk_entries, 1);
    assert_eq!(stats.disk_bytes, 8);
    assert_eq!(stats.reserved_disk_bytes, 0);
    assert_eq!(source.calls.load(Ordering::SeqCst), 2);
}

#[test]
fn stuck_source_deadlines_do_not_release_running_admission_or_allow_reopen() {
    let base = fixture();
    let objects = vec![object(b"aaaaaaaa"), object(b"bbbbbbbb")];
    let source = Source::new("stuck", &objects, true, false);
    let mut limits = small_limits();
    limits.workers = 1;
    limits.max_jobs = 1;
    limits.max_inflight_bytes = 8;
    let cache = open(base.path(), source.clone(), limits.clone());
    let ticket = cache
        .request(&objects[0].0, Instant::now() + Duration::from_millis(50))
        .unwrap();
    until(|| source.active.load(Ordering::SeqCst) == 1);
    assert_eq!(ticket.wait().unwrap_err(), CacheError::TimedOut);
    assert_eq!(cache.stats().unwrap().inflight_jobs, 1);
    assert_eq!(cache.stats().unwrap().inflight_bytes, 8);
    assert_eq!(
        cache.request(&objects[1].0, deadline()).unwrap_err(),
        CacheError::Busy
    );
    drop(ticket);
    drop(cache);
    assert_eq!(
        RemoteCache::open(
            base.path().canonicalize().unwrap(),
            source.clone(),
            limits.clone()
        )
        .unwrap_err(),
        CacheError::Busy
    );
    assert_eq!(source.calls.load(Ordering::SeqCst), 1);
    source.release();
    until(|| source.active.load(Ordering::SeqCst) == 0);
    let cache = reopen(base.path(), source, limits);
    assert_eq!(cache.stats().unwrap().disk_entries, 0);
    assert_eq!(
        fs::read_dir(cache.namespace_root().join("tmp"))
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn future_deadline_wakes_even_when_the_fixed_worker_cannot_return() {
    use std::future::Future;
    use std::task::{Context, Poll, Wake, Waker};
    struct Notifier {
        count: AtomicUsize,
        changed: Condvar,
        mutex: Mutex<()>,
    }
    impl Wake for Notifier {
        fn wake(self: Arc<Self>) {
            self.count.fetch_add(1, Ordering::SeqCst);
            self.changed.notify_all();
        }
    }
    let base = fixture();
    let object = object(b"data");
    let source = Source::new("future", &[object.clone()], true, false);
    let cache = open(base.path(), source.clone(), small_limits());
    let mut ticket = Box::pin(
        cache
            .request(&object.0, Instant::now() + Duration::from_millis(60))
            .unwrap(),
    );
    let notified = Arc::new(Notifier {
        count: AtomicUsize::new(0),
        changed: Condvar::new(),
        mutex: Mutex::new(()),
    });
    let waker = Waker::from(notified.clone());
    let mut context = Context::from_waker(&waker);
    assert!(matches!(ticket.as_mut().poll(&mut context), Poll::Pending));
    let guard = notified.mutex.lock().unwrap();
    drop(
        notified
            .changed
            .wait_timeout_while(guard, Duration::from_secs(1), |_| {
                notified.count.load(Ordering::SeqCst) == 0
            })
            .unwrap(),
    );
    assert!(notified.count.load(Ordering::SeqCst) > 0);
    assert!(matches!(
        ticket.as_mut().poll(&mut context),
        Poll::Ready(Err(CacheError::TimedOut))
    ));
    assert_eq!(cache.stats().unwrap().inflight_jobs, 1);
    drop(ticket);
    source.release();
    until(|| cache.stats().unwrap().inflight_jobs == 0);
}

#[test]
fn corrupt_bytes_sizes_and_metadata_never_escape_and_safe_errors_hide_source_details() {
    let base = fixture();
    let object = object(b"abcdef");
    let source = Source::new("verification", &[object.clone()], false, true);
    let cache = open(base.path(), source.clone(), small_limits());
    for mode in [1, 2, 3] {
        source.mode.store(mode, Ordering::SeqCst);
        assert_eq!(
            cache
                .request(&object.0, deadline())
                .unwrap()
                .wait()
                .unwrap_err(),
            CacheError::Corrupt
        );
        let stats = cache.stats().unwrap();
        assert_eq!(stats.disk_entries, 0);
        assert_eq!(stats.reserved_disk_bytes, 0);
        assert_eq!(stats.inflight_bytes, 0);
    }
    source.mode.store(0, Ordering::SeqCst);
    source.fail.store(true, Ordering::SeqCst);
    let error = cache
        .request(&object.0, deadline())
        .unwrap()
        .wait()
        .unwrap_err();
    assert_eq!(error, CacheError::SourceFailed);
    assert!(!format!("{error:?} {error}").contains("secret"));
    source.fail.store(false, Ordering::SeqCst);
    assert_eq!(read(&cache, &object.0).bytes(), b"abcdef");
    let malformed =
        canonical_json(&serde_json::json!({"format_version":99,"logical_size":1})).unwrap();
    let metadata = ObjectRef::from_bytes(ObjectKind::File, &malformed);
    source
        .objects
        .lock()
        .unwrap()
        .insert(metadata.clone(), malformed);
    assert_eq!(
        cache
            .request(&metadata, deadline())
            .unwrap()
            .wait()
            .unwrap_err(),
        CacheError::Corrupt
    );
}

#[test]
fn borrowed_snapshot_survives_disk_tamper_and_next_cold_acquisition_repairs() {
    use std::os::unix::fs::PermissionsExt;
    let base = fixture();
    let object = object(b"original");
    let source = Source::new("tamper", &[object.clone()], false, true);
    let cache = open(base.path(), source.clone(), small_limits());
    let first = read(&cache, &object.0);
    let path = entry(&cache, &object.0);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    fs::write(&path, b"tampered").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();
    assert_eq!(first.bytes(), b"original");
    let second = read(&cache, &object.0);
    assert_eq!(second.bytes(), b"original");
    assert_eq!(source.calls.load(Ordering::SeqCst), 1);
    drop(first);
    drop(second);
    until(|| cache.stats().unwrap().resident_bytes == 0);
    assert_eq!(read(&cache, &object.0).bytes(), b"original");
    assert_eq!(source.calls.load(Ordering::SeqCst), 2);
    assert_eq!(cache.stats().unwrap().corrupt_entries, 1);
}

#[test]
fn restart_removes_orphans_enforces_reduced_quota_and_rejects_unknown_or_symlink_paths() {
    use std::os::unix::fs::symlink;
    let base = fixture();
    let objects = vec![object(b"aaaaaaaa"), object(b"bbbbbbbb")];
    let source = Source::new("restart", &objects, false, true);
    let limits = small_limits();
    let cache = open(base.path(), source.clone(), limits.clone());
    drop(read(&cache, &objects[0].0));
    drop(read(&cache, &objects[1].0));
    let root = cache.namespace_root().to_owned();
    drop(cache);
    let owner = reopen(base.path(), source.clone(), limits.clone());
    drop(owner);
    drained(&root);
    let orphan = root
        .join("tmp")
        .join(format!("part-{}.tmp", Uuid::new_v4()));
    fs::write(&orphan, b"partial bytes").unwrap();
    let unknown = root.join("entries/unrelated.txt");
    fs::write(&unknown, b"preserve").unwrap();
    assert!(RemoteCache::open(
        base.path().canonicalize().unwrap(),
        source.clone(),
        limits.clone()
    )
    .is_err());
    assert!(unknown.exists());
    assert!(orphan.exists());
    fs::remove_file(&unknown).unwrap();
    let escaped = base.path().join("outside");
    fs::write(&escaped, b"outside sentinel").unwrap();
    let link = root
        .join("entries")
        .join(format!("blob-{}-8.obj", "0".repeat(64)));
    symlink(&escaped, &link).unwrap();
    assert!(RemoteCache::open(
        base.path().canonicalize().unwrap(),
        source.clone(),
        limits.clone()
    )
    .is_err());
    assert_eq!(fs::read(&escaped).unwrap(), b"outside sentinel");
    assert!(orphan.exists());
    fs::remove_file(link).unwrap();
    let mut reduced = limits;
    reduced.disk_quota_bytes = 8;
    reduced.max_disk_entries = 1;
    let cache = reopen(base.path(), source.clone(), reduced);
    assert_eq!(cache.recovery().removed_orphan_temps, 1);
    assert_eq!(cache.recovery().evicted_entries, 1);
    assert_eq!(cache.stats().unwrap().disk_bytes, 8);
    assert_eq!(cache.stats().unwrap().disk_entries, 1);
    source.fail.store(true, Ordering::SeqCst);
    let remaining = fs::read_dir(root.join("entries"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let bytes = fs::read(remaining).unwrap();
    let reference = ObjectRef::from_bytes(ObjectKind::Blob, &bytes);
    assert_eq!(read(&cache, &reference).bytes(), bytes);
    assert_eq!(source.calls.load(Ordering::SeqCst), 2);
}

#[test]
fn provider_and_tenant_scope_are_separate_and_legacy_large_objects_are_rejected() {
    let base = fixture();
    let object = object(b"shared digest, separate authorization");
    let first_source = Source::new("tenant-a", &[object.clone()], false, true);
    let second_source = Source::new("tenant-b", &[], false, true);
    let cache_a = open(base.path(), first_source.clone(), small_limits());
    drop(read(&cache_a, &object.0));
    let cache_b = open(base.path(), second_source.clone(), small_limits());
    assert_ne!(cache_a.namespace_root(), cache_b.namespace_root());
    assert_eq!(
        cache_b
            .request(&object.0, deadline())
            .unwrap()
            .wait()
            .unwrap_err(),
        CacheError::SourceFailed
    );
    assert_eq!(second_source.calls.load(Ordering::SeqCst), 1);
    assert_eq!(first_source.calls.load(Ordering::SeqCst), 1);
    let calls = first_source.calls.load(Ordering::SeqCst);
    for (kind, size) in [
        (ObjectKind::Blob, 4 * 1024 * 1024 + 1),
        (ObjectKind::File, 64 * 1024 + 1),
    ] {
        let invalid = ObjectRef {
            kind,
            size_bytes: size,
            ..object.0.clone()
        };
        assert_eq!(
            cache_a.request(&invalid, deadline()).unwrap_err(),
            CacheError::InvalidReference
        );
    }
    assert_eq!(first_source.calls.load(Ordering::SeqCst), calls);
}

#[test]
fn per_object_schema_is_not_complete_closure_authority_and_does_not_pin_local_store() {
    use rudder_engine::{
        content::{FileNode, FILE_LAYOUT},
        store::LocalStore,
    };
    let base = fixture();
    let missing = ObjectRef::from_bytes(ObjectKind::Blob, b"not downloaded");
    let bytes = canonical_json(&FileNode {
        format_version: 1,
        layout: FILE_LAYOUT.into(),
        level: 0,
        logical_size: missing.size_bytes,
        children: vec![missing.clone()],
    })
    .unwrap();
    let reference = ObjectRef::from_bytes(ObjectKind::File, &bytes);
    let source = Source::new(
        "partial",
        &[(reference.clone(), bytes.clone())],
        false,
        true,
    );
    let cache = open(base.path(), source.clone(), CacheLimits::default());
    let local = LocalStore::open(base.path().join("local-cas")).unwrap();
    let before = local.retention().unwrap();
    // Metadata can be cached without a child or a local root pin. Parent/child
    // semantics and remote-root authorization belong to the caller's resolver.
    assert_eq!(read(&cache, &reference).bytes(), bytes);
    assert_eq!(local.retention().unwrap(), before);
    assert!(local.read_object(&missing).is_err());
    assert_eq!(cache.stats().unwrap().disk_entries, 1);
}

#[test]
fn borrowed_snapshot_holds_namespace_ownership_after_cache_handle_drop() {
    let base = fixture();
    let object = object(b"kept snapshot");
    let source = Source::new("borrowed-lock", &[object.clone()], false, true);
    let limits = small_limits();
    let cache = open(base.path(), source.clone(), limits.clone());
    let data = read(&cache, &object.0);
    drop(cache);
    assert_eq!(
        RemoteCache::open(
            base.path().canonicalize().unwrap(),
            source.clone(),
            limits.clone()
        )
        .unwrap_err(),
        CacheError::Busy
    );
    assert_eq!(data.bytes(), b"kept snapshot");
    drop(data);
    source.fail.store(true, Ordering::SeqCst);
    let cache = reopen(base.path(), source.clone(), limits);
    assert_eq!(read(&cache, &object.0).bytes(), b"kept snapshot");
    assert_eq!(source.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn kind_namespaces_do_not_reuse_a_blob_as_metadata_without_source_authority() {
    use rudder_engine::content::{FileNode, FILE_LAYOUT};
    let base = fixture();
    let child = ObjectRef::from_bytes(ObjectKind::Blob, b"x");
    let bytes = canonical_json(&FileNode {
        format_version: 1,
        layout: FILE_LAYOUT.into(),
        level: 0,
        logical_size: 1,
        children: vec![child],
    })
    .unwrap();
    let blob = ObjectRef::from_bytes(ObjectKind::Blob, &bytes);
    let file = ObjectRef::from_bytes(ObjectKind::File, &bytes);
    let source = Source::new("kind", &[(blob.clone(), bytes.clone())], false, true);
    let cache = open(base.path(), source.clone(), CacheLimits::default());
    let first = read(&cache, &blob);
    assert_eq!(first.reference().kind, ObjectKind::Blob);
    assert_eq!(
        cache
            .request(&file, deadline())
            .unwrap()
            .wait()
            .unwrap_err(),
        CacheError::SourceFailed
    );
    source
        .objects
        .lock()
        .unwrap()
        .insert(file.clone(), bytes.clone());
    let second = read(&cache, &file);
    assert_eq!(second.reference().kind, ObjectKind::File);
    assert_eq!(second.bytes(), bytes);
    assert_eq!(source.calls.load(Ordering::SeqCst), 3);
    assert_eq!(cache.stats().unwrap().disk_entries, 2);
}

#[test]
fn source_panic_cleans_reserved_temporary_storage_and_the_fixed_pool_recovers() {
    let base = fixture();
    let object = object(b"after panic");
    let source = Source::new("panic", &[object.clone()], false, true);
    source.mode.store(4, Ordering::SeqCst);
    let cache = open(base.path(), source.clone(), small_limits());
    assert_eq!(
        cache
            .request(&object.0, deadline())
            .unwrap()
            .wait()
            .unwrap_err(),
        CacheError::SourceFailed
    );
    let stats = cache.stats().unwrap();
    assert_eq!(stats.reserved_disk_bytes, 0);
    assert_eq!(stats.reserved_disk_entries, 0);
    assert_eq!(stats.pending_resident_bytes, 0);
    assert!(!stats.poisoned);
    assert_eq!(
        fs::read_dir(cache.namespace_root().join("tmp"))
            .unwrap()
            .count(),
        0
    );
    source.mode.store(0, Ordering::SeqCst);
    assert_eq!(read(&cache, &object.0).bytes(), b"after panic");
}

#[test]
fn startup_scan_limit_fails_without_sweeping_uninspected_entries() {
    let base = fixture();
    let objects = vec![object(b"a"), object(b"b"), object(b"c")];
    let source = Source::new("scan", &objects, false, true);
    let limits = small_limits();
    let cache = open(base.path(), source.clone(), limits.clone());
    for object in &objects {
        drop(read(&cache, &object.0));
    }
    let root = cache.namespace_root().to_owned();
    drop(cache);
    let owner = reopen(base.path(), source.clone(), limits.clone());
    drop(owner);
    let mut bounded = limits.clone();
    bounded.max_disk_entries = 1;
    bounded.max_recovery_entries = 2;
    drained(&root);
    assert_eq!(
        RemoteCache::open(base.path().canonicalize().unwrap(), source.clone(), bounded)
            .unwrap_err(),
        CacheError::UnsafePath
    );
    assert_eq!(fs::read_dir(root.join("entries")).unwrap().count(), 3);
    let cache = reopen(base.path(), source, limits);
    assert_eq!(cache.recovery().recovered_entries, 3);
}

#[test]
fn maximum_chunk_is_streamed_and_verified_with_bounded_cache_accounting() {
    let base = fixture();
    let object = object(&vec![0x5a; 4 * 1024 * 1024]);
    let source = Source::new("max-chunk", &[object.clone()], false, true);
    let mut limits = CacheLimits::default();
    limits.max_inflight_bytes = 4 * 1024 * 1024;
    limits.max_resident_bytes = 4 * 1024 * 1024;
    limits.max_borrowed_bytes = 4 * 1024 * 1024;
    let cache = open(base.path(), source, limits);
    let data = read(&cache, &object.0);
    assert_eq!(data.bytes(), object.1);
    let stats = cache.stats().unwrap();
    assert_eq!(stats.resident_bytes, 4 * 1024 * 1024);
    assert_eq!(stats.borrowed_bytes, 4 * 1024 * 1024);
    assert_eq!(stats.inflight_bytes, 0);
    assert_eq!(stats.pending_resident_bytes, 0);
    assert_eq!(data.slice(4 * 1024 * 1024 - 2, 2).unwrap(), [0x5a; 2]);
    assert!(data.slice(u64::MAX, 1).is_err());
}

struct AbortSource {
    ack: PathBuf,
    partial: bool,
}
impl ImmutableObjectSource for AbortSource {
    fn scope(&self) -> CacheScope {
        CacheScope {
            provider_id: "fixture-provider".into(),
            tenant_id: "abort".into(),
        }
    }
    fn fetch(&self, _: &ObjectRef, _: &FetchControl, output: &mut dyn Write) -> anyhow::Result<()> {
        if self.partial {
            output.write_all(b"part")?;
            fs::write(&self.ack, b"partial")?;
            std::process::abort();
        }
        output.write_all(b"acknowledged-cache-bytes")?;
        Ok(())
    }
}
#[test]
fn remote_cache_abort_child() {
    let Ok(base) = std::env::var("RUDDER_REMOTE_CACHE_ABORT_ROOT") else {
        return;
    };
    unsafe {
        let limits = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        libc::setrlimit(libc::RLIMIT_CORE, &limits);
    }
    let partial = std::env::var("RUDDER_REMOTE_CACHE_ABORT_MODE").unwrap() == "partial";
    let base = PathBuf::from(base);
    let source = Arc::new(AbortSource {
        ack: base.join("ack"),
        partial,
    });
    let cache = RemoteCache::open(base.canonicalize().unwrap(), source, small_limits()).unwrap();
    let reference = ObjectRef::from_bytes(ObjectKind::Blob, b"acknowledged-cache-bytes");
    let bytes = cache
        .request(&reference, deadline())
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(bytes.bytes(), b"acknowledged-cache-bytes");
    fs::write(base.join("ack"), b"complete").unwrap();
    std::process::abort();
}
#[test]
fn process_abort_recovery_discards_unacknowledged_temp_and_keeps_verified_offline_entry() {
    for mode in ["partial", "complete"] {
        let base = fixture();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "remote_cache_abort_child", "--nocapture"])
            .env("RUDDER_REMOTE_CACHE_ABORT_ROOT", base.path())
            .env("RUDDER_REMOTE_CACHE_ABORT_MODE", mode)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert_eq!(fs::read(base.path().join("ack")).unwrap(), mode.as_bytes());
        let object = object(b"acknowledged-cache-bytes");
        let source = Source::new("abort", &[object.clone()], false, true);
        source.fail.store(true, Ordering::SeqCst);
        let cache = open(base.path(), source.clone(), small_limits());
        if mode == "partial" {
            assert_eq!(cache.recovery().removed_orphan_temps, 1);
            assert_eq!(cache.recovery().recovered_entries, 0);
            assert_eq!(
                cache
                    .request(&object.0, deadline())
                    .unwrap()
                    .wait()
                    .unwrap_err(),
                CacheError::SourceFailed
            );
        } else {
            assert_eq!(cache.recovery().recovered_entries, 1);
            assert_eq!(read(&cache, &object.0).bytes(), object.1);
            assert_eq!(source.calls.load(Ordering::SeqCst), 0);
        }
    }
}

#[test]
fn cooperative_cancellation_releases_only_after_source_ack_and_allows_new_work() {
    let base = fixture();
    let object = object(b"cancelled download");
    let source = Source::new("cooperative", &[object.clone()], true, true);
    let mut limits = small_limits();
    limits.max_jobs = 1;
    limits.workers = 1;
    let cache = open(base.path(), source.clone(), limits);
    let ticket = cache.request(&object.0, deadline()).unwrap();
    until(|| source.active.load(Ordering::SeqCst) == 1);
    ticket.cancel();
    assert_eq!(ticket.wait().unwrap_err(), CacheError::Cancelled);
    drop(ticket);
    until(|| cache.stats().unwrap().inflight_jobs == 0);
    let stats = cache.stats().unwrap();
    assert_eq!(stats.inflight_bytes, 0);
    assert_eq!(stats.pending_resident_bytes, 0);
    assert_eq!(stats.reserved_disk_bytes, 0);
    assert_eq!(source.active.load(Ordering::SeqCst), 0);
    source.release();
    assert_eq!(read(&cache, &object.0).bytes(), b"cancelled download");
    assert_eq!(source.calls.load(Ordering::SeqCst), 2);
}

#[test]
fn concurrent_request_drop_and_eviction_preserve_all_independent_caps() {
    let base = fixture();
    let objects = (0..8).map(|i| object(&[i; 16])).collect::<Vec<_>>();
    let source = Source::new("concurrent", &objects, false, true);
    let mut limits = small_limits();
    limits.workers = 2;
    limits.max_jobs = 4;
    limits.max_tickets = 64;
    limits.max_inflight_bytes = 64;
    limits.max_resident_bytes = 128;
    limits.max_borrowed_bytes = 64;
    let cache = open(base.path(), source.clone(), limits);
    let mut workers = Vec::new();
    for thread in 0..8 {
        let cache = cache.clone();
        let objects = objects.clone();
        workers.push(std::thread::spawn(move || {
            for step in 0..40 {
                let object = &objects[(thread + step) % objects.len()];
                let end = Instant::now() + Duration::from_secs(3);
                loop {
                    assert!(Instant::now() < end, "bounded concurrent request starved");
                    let result = match cache.request(&object.0, deadline()) {
                        Ok(ticket) => ticket.wait(),
                        Err(error) => Err(error),
                    };
                    match result {
                        Ok(bytes) => {
                            assert_eq!(bytes.bytes(), object.1);
                            break;
                        }
                        Err(CacheError::Busy) => std::thread::yield_now(),
                        Err(error) => panic!("unexpected concurrent request:{error}"),
                    }
                }
                let stats = cache.stats().unwrap();
                assert!(stats.inflight_jobs <= 4);
                assert!(stats.inflight_bytes <= 64);
                assert!(stats.resident_bytes + stats.pending_resident_bytes <= 128);
                assert!(stats.borrowed_bytes <= 64);
                assert!(stats.disk_bytes + stats.reserved_disk_bytes <= 256);
                assert!(stats.disk_entries + stats.reserved_disk_entries <= 16);
            }
        }));
    }
    for worker in workers {
        worker.join().unwrap();
    }
    until(|| cache.stats().unwrap().inflight_jobs == 0);
    assert!(source.peak.load(Ordering::SeqCst) <= 2);
    assert_eq!(source.calls.load(Ordering::SeqCst), 8);
}
