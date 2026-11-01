//! Pure/local-CAS tests run normally. The TWO ignored tests are paired: invoke
//! ONLY root_seed_mount_kernel_qualification explicitly on a disposable root VM.
//! It launches the other test through fixed setpriv; direct child invocation
//! without the actual parent-owned fixture is an error, never an implicit skip.
use super::*;
use rudder_engine::protocol::{CheckpointConsistency, OwnerEpoch};
use rudder_vcs_bridge::{AttemptBinding, CommitIdentity, GitCommitId, GitTreeId,
    JjChangeId, JjOperationId, JjTreeIdentity, seed::{prepare_metadata_template, fork_private_metadata}};
use std::{process::{Child, Command, Stdio}, time::Duration};

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let base = if Path::new("/workspace").is_dir() { PathBuf::from("/workspace") } else { std::env::temp_dir() };
        let path = base.join(format!("rudder-root-seed-unit-{}", uuid::Uuid::new_v4()));
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap(); Self(path)
    }
}
impl Drop for Scratch { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); } }
fn baseline() -> CommitIdentity {
    CommitIdentity { commit: GitCommitId("a".repeat(40)), change: JjChangeId("b".repeat(32)),
        parents: vec![GitCommitId("c".repeat(40))], tree: JjTreeIdentity { terms: vec![GitTreeId("d".repeat(40))], labels: Vec::new() } }
}
fn source_fixture(root: &Path) -> Result<(LocalStore, ObjectRef)> {
    let source = root.join("private");
    for part in [".git/objects/aa", ".jj/repo/store", ".jj/working_copy"] {
        fs::create_dir_all(source.join(part))?;
    }
    fs::write(source.join(".git/HEAD"), b"ref: refs/heads/main\n")?;
    fs::write(source.join(".git/objects/aa/fixture"), b"immutable-object-fixture")?;
    fs::write(source.join(".jj/repo/store/git_target"), b"../../../.git")?;
    fs::write(source.join(".jj/working_copy/type"), b"local")?;
    prepare_metadata_template(&source, &root.join("export"), baseline(), JjOperationId("e".repeat(128)))?;
    let store = LocalStore::open(root.join("root-cas"))?;
    let seed = store.import_tree(root.join("export"), CheckpointConsistency::Quiesced)?;
    store.pin("root-fixture-original", &seed)?; Ok((store, seed))
}

#[test]
fn actual_cas_seed_plan_preserves_modes_and_exact_manifest_namespace() {
    let root = Scratch::new(); let (store, seed) = source_fixture(&root.0).unwrap();
    let p = plan(&store, &seed, &|| Ok(())).unwrap();
    assert_eq!(p.files.len(), 6); // manifest + four metadata files + one object
    assert_eq!(p.bytes, p.files.iter().map(|f| f.descriptor.size_bytes).sum::<u64>());
    assert!(p.files.iter().all(|file| file.mode == 0o600));
    assert!(p.directories.iter().all(|(_,mode)| *mode == 0o700));
    assert!(p.files.iter().any(|f| f.path == Path::new("objects/aa/fixture")));
    assert_eq!(store.retention().unwrap().pins.get("root-fixture-original"), Some(&seed));
}

#[test]
fn oversized_initial_legacy_directory_is_refused_before_manifest_lookup() {
    let root = Scratch::new(); let store = LocalStore::open(root.0.join("cas")).unwrap();
    let blob = store.put_blob(b"").unwrap();
    let entries = (0..1024).map(|n| {
        let name = format!("{n:04}-{}", "x".repeat(92));
        rudder_engine::store::DirectoryEntry::new(name.as_bytes(),
            rudder_engine::store::EntryNode::File { object: blob.clone(), mode: 0o600 }).unwrap()
    }).collect();
    let seed = store.put_directory(rudder_engine::store::DirectoryManifest {
        format_version: 1, mode: 0o700, entries }).unwrap();
    assert!(seed.size_bytes > MAX_MANIFEST && seed.size_bytes <= 2 * MAX_MANIFEST);
    // This is a valid actual CAS directory accepted by the general legacy
    // decoder. Small-profile preparation must refuse before that decode.
    assert!(tree::lookup(&store, &seed, b"manifest.json").unwrap().is_none());
    let error = plan(&store, &seed, &|| Ok(())).err().unwrap();
    assert_eq!(error.to_string(), "initial root seed directory exceeds small profile");
}

#[test]
fn late_actual_cas_validation_cannot_outlive_original_cancel_or_budget() {
    let root = Scratch::new(); let (store, _) = source_fixture(&root.0).unwrap();
    for cancel_budget in [false, true] {
        let budget = FinalizationBudget::begin(Duration::from_secs(5)).unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let lock = OpenOptions::new().read(true).write(true).open(store.root().join("lock")).unwrap();
        fs2::FileExt::lock_exclusive(&lock).unwrap();
        let (entered, started) = std::sync::mpsc::sync_channel(1);
        let original = budget.clone(); let cancel = Arc::clone(&cancelled);
        let release = std::thread::spawn(move || {
            started.recv_timeout(Duration::from_secs(5)).unwrap();
            if cancel_budget { original.cancel(); } else { cancel.store(true, Ordering::Release); }
            fs2::FileExt::unlock(&lock).unwrap();
        });
        let mut actual_validation_returned = false;
        let result = validate_with_original_budget(&budget, &cancelled, || {
            // Signal only AFTER the helper's initial admission checkpoint.
            // The real retention lock cannot return until the original
            // cancellation/budget has been invalidated by the other thread.
            entered.send(()).unwrap(); store.retention()?;
            actual_validation_returned = true; Ok(())
        });
        release.join().unwrap();
        assert!(actual_validation_returned && result.is_err());
    }
}

#[test]
fn actual_cas_extra_file_and_symlink_fail_before_mount() {
    let root = Scratch::new(); let (store, _) = source_fixture(&root.0).unwrap();
    fs::write(root.0.join("export/objects/unlisted"), b"unlisted").unwrap();
    let extra = store.import_tree(root.0.join("export"), CheckpointConsistency::Quiesced).unwrap();
    assert!(plan(&store, &extra, &|| Ok(())).is_err());
    fs::remove_file(root.0.join("export/objects/unlisted")).unwrap();
    std::os::unix::fs::symlink("/etc/passwd", root.0.join("export/objects/escape")).unwrap();
    let link = store.import_tree(root.0.join("export"), CheckpointConsistency::Quiesced).unwrap();
    assert!(plan(&store, &link, &|| Ok(())).is_err());
}

#[test]
fn actual_manifest_count_and_original_cancel_stop_plan_without_effects() {
    let root = Scratch::new(); let (store, seed) = source_fixture(&root.0).unwrap();
    let cancelled = AtomicBool::new(true);
    let budget = FinalizationBudget::begin(Duration::from_secs(1)).unwrap();
    assert!(plan(&store, &seed, &|| checkpoint(&budget, &cancelled)).is_err());
    let file = root.0.join("export/manifest.json");
    let mut manifest: MetadataTemplate = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    let descriptor = manifest.metadata[".git/HEAD"].clone();
    for n in 0..MAX_FILES { manifest.metadata.insert(format!(".git/refs/heads/branch{n}"), descriptor.clone()); }
    fs::write(file, canonical_json(&manifest).unwrap()).unwrap();
    let over = store.import_tree(root.0.join("export"), CheckpointConsistency::Quiesced).unwrap();
    assert!(plan(&store, &over, &|| Ok(())).is_err());
}

#[test]
fn small_profile_refuses_path_aliases_and_expanded_allocation() {
    for name in ["../escape", "/absolute", "a//b", "a/./b", "a/../b", "a\\b", "a\nb"] {
        assert!(relative(name).is_err(), "{name}");
    }
    assert!(relative("metadata/.jj/repo/store/git_target").is_ok());
    let root = Scratch::new(); let (store, _) = source_fixture(&root.0).unwrap();
    // Thousands of empty directories consume real tmpfs pages/inodes even
    // without logical bytes; count expanded names rather than unique digests.
    for group in 0..64 {
        let directory = root.0.join(format!("export/objects/group{group}"));
        fs::DirBuilder::new().mode(0o700).create(&directory).unwrap();
        for n in 0..64 { fs::DirBuilder::new().mode(0o700).create(directory.join(format!("d{n}"))).unwrap(); }
    }
    let expanded = store.import_tree(root.0.join("export"), CheckpointConsistency::Quiesced).unwrap();
    assert!(plan(&store, &expanded, &|| Ok(())).is_err());
}

#[test]
fn actual_namespace_and_boot_descriptors_are_held_and_cloexec() {
    let mount = mount_namespace().unwrap(); let second = mount_namespace().unwrap();
    assert_eq!(identity(&mount).unwrap(), identity(&second).unwrap());
    assert!(unsafe { libc::fcntl(mount.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC != 0);
    let file = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open("/proc/sys/kernel/random/boot_id").unwrap();
    assert_eq!(read_boot(&file).unwrap(), fs::read("/proc/sys/kernel/random/boot_id").unwrap());
    assert!(user_namespace().is_ok());
}

fn wait_owned(child: &mut Child, budget: &FinalizationBudget) -> Result<std::process::ExitStatus> {
    loop {
        budget.remaining()?;
        let status = child.try_wait()?; budget.remaining()?;
        if let Some(status) = status { return Ok(status); }
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn chown_new(path: &Path) -> Result<()> {
    let path = CString::new(path.as_os_str().as_bytes())?;
    syscall(unsafe { libc::chown(path.as_ptr(), READER_UID, READER_GID) }, "qualification fresh child directory chown")
}

#[test]
#[ignore = "Root-only physical qualification; exact filter on a disposable pinned worker"]
fn root_seed_mount_kernel_qualification() -> Result<()> {
    let budget = FinalizationBudget::begin(Duration::from_secs(30))?;
    let cancelled = AtomicBool::new(false);
    let work = Path::new("/run").join(format!("rudder-root-seed-qualification-{}", uuid::Uuid::new_v4()));
    fs::DirBuilder::new().mode(0o700).create(&work)?;
    let (store, seed) = source_fixture(&work)?;
    let mut owner = RootSeedMount::capture(store.clone(), seed.clone())?;
    // Carrier, original read owner, and actual Child stay outside the inner
    // operation. A failure cannot turn dropped lifetime tokens into drain.
    let mut read = None; let mut child: Option<Child> = None; let mut reaped = false;
    let result = (|| -> Result<serde_json::Value> {
        owner.prepare(&budget, &cancelled)?;
        read = Some(owner.acquire_read()?);
        let held = read.as_ref().unwrap(); held.template()?;
        let seed_path = held.root().to_path_buf(); let objects = held.objects();
        let pin = owner.resources.source.name.clone();
        ensure!(store.retention()?.pins.get(&pin) == Some(&seed), "qualification root source pin absent");
        ensure!(owner.cleanup_after_reader_drain(&budget).is_err() && !owner.is_settled(), "live reader did not block cleanup");
        ensure!(mount_at(&seed_path)?.is_some() && store.retention()?.pins.contains_key(&pin), "blocked cleanup discarded mount/pin");
        let child_root = work.join("child"); fs::DirBuilder::new().mode(0o700).create(&child_root)?; chown_new(&child_root)?;
        // Root CAS remains private. Only fresh qualification parent traversal
        // and this independent child's scratch are made available to UID1500.
        fs::set_permissions(&work, fs::Permissions::from_mode(0o711))?;
        let executable = std::env::current_exe()?.canonicalize()?;
        let child_test = format!("{}::root_seed_mount_uid1500_child", module_path!().split_once("::").unwrap().1);
        let receipt = child_root.join("receipt.json");
        child = Some(Command::new("/usr/bin/setpriv")
            .args(["--reuid=1500", "--regid=1500", "--clear-groups", "--inh-caps=-all", "--ambient-caps=-all", "--bounding-set=-all", "--no-new-privs", "--"])
            .arg(&executable).args(["--ignored", "--exact", &child_test, "--test-threads=1"])
            .env_clear().env("RUDDER_SEED_QUAL_ROOT", &seed_path).env("RUDDER_SEED_QUAL_CHILD", &child_root)
            .env("RUDDER_SEED_QUAL_REF", String::from_utf8(canonical_json(&seed)?)?)
            .env("RUDDER_SEED_QUAL_MNT", fs::read_link("/proc/thread-self/ns/mnt")?)
            .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?);
        let status = wait_owned(child.as_mut().unwrap(), &budget)?; reaped = true;
        ensure!(status.success(), "actual UID1500 qualification child failed: {status}");
        let file = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(&receipt)?;
        let metadata = file.metadata()?;
        ensure!(metadata.is_file() && metadata.uid() == 1500 && metadata.gid() == 1500 && metadata.nlink() == 1
            && metadata.len() <= 16384, "qualification child receipt invalid");
        let mut bytes = Vec::new(); file.take(16385).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 16384, "qualification child receipt exceeds bound");
        let proof: serde_json::Value = serde_json::from_slice(&bytes)?;
        ensure!(proof["seed"] == serde_json::to_value(&seed)? && proof["uid"] == 1500 && proof["caps0_nnp1"] == true
            && proof["same_mount_namespace"] == true && proof["write_error"] == "EROFS"
            && proof["private_forks"] == 2 && proof["child_cas_owner"] == 1500, "qualification child proof differs");
        ensure!(objects == seed_path.join("objects"), "qualification global object path changed");
        Ok(proof)
    })();
    // Failed/late child still needs actual kill+reap before any cleanup. A
    // missed recovery budget leaves explicit kernel/pin state for VM teardown.
    if child.is_some() && !reaped {
        let held = child.as_mut().unwrap(); let _ = held.kill();
        let recovery = FinalizationBudget::begin(Duration::from_secs(3))?;
        reaped = wait_owned(held, &recovery).is_ok();
    }
    if child.is_some() && !reaped {
        eprintln!("root seed qualification recovery required: mount/pin retained; actual reader reap unconfirmed");
        return Err(anyhow::anyhow!("unsettled qualification child; owned VM cleanup required"));
    }
    drop(read.take());
    let cleanup = FinalizationBudget::begin(Duration::from_secs(5))?;
    if result.is_ok() {
        // A real retained kernel reader independently exercises EBUSY. A
        // failed normal unmount must preserve the same source pin and carrier.
        let busy = open_dir(&owner.resources.target.join("objects"))?;
        let failure = owner.cleanup_after_reader_drain(&cleanup).expect_err("actual busy kernel reader must refuse unmount");
        ensure!(failure.downcast_ref::<std::io::Error>().is_some_and(|e| e.raw_os_error() == Some(libc::EBUSY))
            && !owner.is_settled() && mount_at(&owner.resources.target)?.is_some()
            && store.retention()?.pins.get(&owner.resources.source.name) == Some(&seed), "EBUSY did not retain actual mount/pin");
        drop(busy);
    }
    owner.cleanup_after_reader_drain(&cleanup)?;
    ensure!(owner.is_settled() && mount_at(&owner.resources.target)?.is_none()
        && !store.retention()?.pins.contains_key(&owner.resources.source.name), "qualification cleanup unconfirmed");
    let proof = result?;
    // Fresh second owner: cancellation is not a cache hit and still needs
    // explicit unused cleanup; the first source pin remains independent.
    let mut failed = RootSeedMount::capture(store.clone(), seed.clone())?;
    let cancelled = AtomicBool::new(true);
    ensure!(failed.prepare(&cleanup, &cancelled).is_err() && !failed.is_settled(), "cancelled prepare falsely settled");
    failed.cleanup_after_reader_drain(&cleanup)?; ensure!(failed.is_settled(), "unused owner did not settle");
    fs::remove_dir_all(&work)?;
    println!("{}", serde_json::json!({"scope":"kernel-seed-producer-and-cross-uid-verifier-only",
        "root_source_pin_retained_through_child":true,"root_write_error":"EROFS","read_owner_blocked_cleanup":true,
        "actual_child_reaped":true,"actual_ebusy_retained_owner":true,"normal_unmount_verified":true,"own_pin_removed":true,
        "cancelled_prepare_settled_explicitly":true,"production_ipc":false,"backend":false,"child":proof}));
    Ok(())
}

#[test]
#[ignore = "Only the paired actual root owner may invoke this through fixed setpriv"]
fn root_seed_mount_uid1500_child() -> Result<()> {
    ensure!(unsafe { libc::getuid() } == 1500 && unsafe { libc::geteuid() } == 1500
        && unsafe { libc::getgid() } == 1500 && unsafe { libc::getegid() } == 1500, "qualification UID1500 required");
    let status = String::from_utf8(proc_bytes("/proc/thread-self/status", 16384)?)?;
    for key in ["CapEff", "CapPrm", "CapInh", "CapAmb", "CapBnd"] {
        let prefix = format!("{key}:");
        let value = status.lines().find_map(|l| l.strip_prefix(prefix.as_str())).context("child capability field missing")?.trim();
        ensure!(!value.is_empty() && value.bytes().all(|b| b == b'0'), "qualification child capabilities remain");
    }
    ensure!(status.lines().any(|l| l.strip_prefix("NoNewPrivs:").is_some_and(|v| v.trim() == "1"))
        && status.lines().any(|l| l.strip_prefix("Groups:").is_some_and(|v| v.trim().is_empty())), "qualification child NNP/groups differ");
    let root = PathBuf::from(std::env::var_os("RUDDER_SEED_QUAL_ROOT").context("original root fixture absent")?);
    let scratch = PathBuf::from(std::env::var_os("RUDDER_SEED_QUAL_CHILD").context("original child fixture absent")?);
    ensure!(fs::read_link("/proc/thread-self/ns/mnt")? == PathBuf::from(std::env::var_os("RUDDER_SEED_QUAL_MNT").context("original MNT absent")?), "qualification namespace differs");
    let expected: ObjectRef = serde_json::from_str(&std::env::var("RUDDER_SEED_QUAL_REF")?)?;
    let store = LocalStore::open(scratch.join("cas"))?;
    ensure!(fs::metadata(store.root())?.uid() == 1500, "qualification did not create actual child CAS");
    let seed = store.import_tree(&root, CheckpointConsistency::Quiesced)?;
    ensure!(seed == expected, "independent child CAS did not reproduce exact seed identity");
    store.pin("actual-child-original", &seed)?;
    let verified = VerifiedMetadataTemplate::open_retained(&store, &seed, &root, &root.join("objects"))?;
    let object = root.join("objects/aa/fixture");
    ensure!(fs::metadata(&object)?.uid() == 1500 && fs::metadata(&object)?.gid() == 1500
        && fs::metadata(&object)?.mode() & 0o777 == 0o600, "fresh sealed file ownership/mode differs");
    let original = fs::read(&object)?;
    let write = OpenOptions::new().append(true).open(&object);
    ensure!(write.as_ref().err().and_then(|e|e.raw_os_error()) == Some(libc::EROFS)
        && fs::read(&object)? == original, "actual UID1500 seed append did not fail EROFS");
    let binding = |id: &str| AttemptBinding { format_version: 1, project_id: "root-seed-kernel-fixture".into(),
        owner: OwnerEpoch { owner: "fixture".into(), epoch: 1 }, run_id: id.into(), run_epoch: 1,
        attempt_id: id.into(), view_id: id.into(), seed: seed.clone(), baseline: baseline(),
        baseline_operation: JjOperationId("e".repeat(128)), projection_policy: ObjectRef::from_bytes(ObjectKind::Blob, b"fixture-policy"),
        profile: ObjectRef::from_bytes(ObjectKind::Blob, b"fixture-profile") };
    for id in ["fork-a", "fork-b"] {
        let receipt = fork_private_metadata(&verified, &scratch.join(id), binding(id), None)?;
        ensure!(receipt.kernel_readonly_objects && receipt.binding.seed == seed, "actual private fork lost RO/seed binding");
        ensure!(fs::read_to_string(scratch.join(id).join(".git/objects/info/alternates"))?
            == format!("{}\n", root.join("objects").display()), "global Git alternate path differs");
    }
    let before = fs::read(scratch.join("fork-b/.git/HEAD"))?;
    fs::write(scratch.join("fork-a/.git/HEAD"), b"independent-private-head\n")?;
    ensure!(fs::read(scratch.join("fork-b/.git/HEAD"))? == before
        && fs::read(root.join("metadata/.git/HEAD"))? == before, "fork shared mutable metadata");
    let receipt = canonical_json(&serde_json::json!({"seed":seed,"uid":1500,"caps0_nnp1":true,
        "same_mount_namespace":true,"write_error":"EROFS","private_forks":2,"child_cas_owner":1500}))?;
    let mut file = OpenOptions::new().write(true).create_new(true).mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(scratch.join("receipt.json"))?;
    file.write_all(&receipt)?; file.sync_all()?; Ok(())
}
