//! Paired ignored qualification of actual Git/jj history through the root RO
//! producer. Invoke ONLY the root test on an exact-owned disposable VM.
use super::*;
use jj_lib::{backend::{CommitId, CopyId, TreeValue}, config::{ConfigLayer, ConfigSource, StackedConfig},
    git_backend::GitBackend, merge::Merge, merged_tree::MergedTree,
    merged_tree_builder::MergedTreeBuilder, object_id::ObjectId as _, op_store::RefTarget,
    ref_name::{RefName, WorkspaceName}, repo::{ReadonlyRepo, Repo, RepoLoader, StoreFactories},
    repo_path::RepoPathBuf, settings::UserSettings, workspace::Workspace};
use rudder_engine::{execution::finalization_budget::FinalizationConstraint,
    protocol::{CheckpointConsistency, OwnerEpoch}, snapshot::{decode_runnable, decode_source, EnvironmentV2},
    store::DirectoryManifest, workspace::WorkspaceProfile};
use rudder_vcs_bridge::{AttemptBinding, CommitIdentity, GitCommitId, JjOperationId,
    initial_source::{prepare_initial_source, InitialSourceRequest}, publication::INTEGRATION_BOOKMARK,
    seed::fork_private_metadata, vfs_provider::SourceProjectionPolicy};
use std::{collections::BTreeMap, process::{Child, Command, ExitStatus, Stdio}, time::Duration};

const PARENT_BYTES: &[u8] = b"real accepted parent bytes\n";
const ACCEPTED_BYTES: &[u8] = b"real accepted child bytes $Format:%H$\n";
const MUTATED_BYTES: &[u8] = b"private first fork changed bytes\n";
const SCOPE: &str = "real-history-seed-producer-and-cross-uid-forks-only";

fn fixture_settings() -> Result<UserSettings> {
    let mut config = StackedConfig::with_defaults();
    let mut layer = ConfigLayer::empty(ConfigSource::User);
    layer.set_value("user.name", "Rudder Real Seed Fixture")?;
    layer.set_value("user.email", "real-seed@invalid.example")?;
    config.add_layer(layer); Ok(UserSettings::from_config(config)?)
}
fn remaining(budget: &FinalizationBudget) -> Result<()> { budget.remaining()?; Ok(()) }
fn write_head(path: &Path, head: &GitCommitId) -> Result<()> {
    head.validate()?;
    let mut file = OpenOptions::new().write(true).truncate(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(path)?;
    ensure!(file.metadata()?.is_file() && file.metadata()?.nlink() == 1, "fixture HEAD must be its private regular file");
    file.write_all(format!("{}\n", head.0).as_bytes())?; file.sync_all()?; Ok(())
}
async fn changed_tree(repo: &Arc<ReadonlyRepo>, base: MergedTree, bytes: &[u8], budget: &FinalizationBudget) -> Result<MergedTree> {
    remaining(budget)?;
    let path = RepoPathBuf::from_internal_string("tracked")?;
    let id = repo.store().write_file(&path, &mut &bytes[..]).await?;
    remaining(budget)?;
    let mut tree = MergedTreeBuilder::new(base);
    tree.set_or_remove(path, Merge::resolved(Some(TreeValue::File {
        id, executable: true, copy_id: CopyId::placeholder(),
    })));
    let tree = tree.write_tree().await?; remaining(budget)?; Ok(tree)
}

/// Test-owned real source lease: held directory/lock descriptors, Workspace and
/// repo owner remain outside the operations through child reap and unmount.
/// This is no EngineSourceLease or accepted Journal authority claim.
struct SourceOwner {
    _workspace: Workspace, repository: Arc<ReadonlyRepo>, directory: HeldDir, git: HeldDir,
    lock: File, lock_path: PathBuf, lock_id: (u64, u64), head: CommitIdentity,
}
impl SourceOwner {
    fn check(&self) -> Result<()> {
        self.directory.check()?; self.git.check()?;
        let named = fs::symlink_metadata(&self.lock_path)?; let held = self.lock.metadata()?;
        for m in [&named, &held] {
            ensure!(m.is_file() && !m.file_type().is_symlink() && (m.dev(), m.ino()) == self.lock_id
                && m.uid() == 0 && m.gid() == 0 && m.nlink() == 1 && m.mode() & 0o7777 == 0o600,
                "real source fixture lock custody changed");
        }
        ensure!(fs::read(self.git.path.join("HEAD"))? == format!("{}\n", self.head.commit.0).as_bytes(),
            "real source fixture accepted HEAD changed");
        let backend = self.repository.store().backend_impl::<GitBackend>().context("fixture Git backend absent")?;
        ensure!(backend.git_repo().head()?.id().context("fixture Git HEAD unborn")?.detach().to_string() == self.head.commit.0,
            "real source fixture Git HEAD differs"); Ok(())
    }
}
async fn make_source(work: &Path, settings: &UserSettings, budget: &FinalizationBudget) -> Result<SourceOwner> {
    remaining(budget)?;
    let path = work.join("original-source"); fs::DirBuilder::new().mode(0o700).create(&path)?;
    let (workspace, repo) = Workspace::init_internal_git(settings, &path).await?;
    remaining(budget)?;
    let empty = MergedTree::resolved(repo.store().clone(), repo.store().empty_tree_id().clone());
    let parent_tree = changed_tree(&repo, empty, PARENT_BYTES, budget).await?;
    let mut tx = repo.start_transaction();
    let parent = tx.repo_mut().new_commit(vec![repo.store().root_commit_id().clone()], parent_tree)
        .set_description("real original fixture parent").clear_sign_key().write().await?;
    remaining(budget)?;
    let tree = changed_tree(&repo, parent.tree(), ACCEPTED_BYTES, budget).await?;
    let head = tx.repo_mut().new_commit(vec![parent.id().clone()], tree)
        .set_description("real original accepted fixture child").clear_sign_key().write().await?;
    tx.repo_mut().set_local_bookmark_target(RefName::new(INTEGRATION_BOOKMARK), RefTarget::normal(head.id().clone()));
    tx.repo_mut().set_wc_commit(WorkspaceName::DEFAULT.to_owned(), head.id().clone())?;
    let repo = tx.commit("publish original fixture history").await?;
    let baseline = CommitIdentity::from_jj(&head);
    let git_path = repo.store().backend_impl::<GitBackend>().context("fixture source Git absent")?.git_repo_path().to_path_buf();
    write_head(&git_path.join("HEAD"), &baseline.commit)?;
    let lock_path = work.join("original-source.lock");
    let lock = OpenOptions::new().read(true).write(true).create_new(true).mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(&lock_path)?;
    fs2::FileExt::try_lock_exclusive(&lock)?;
    let source = SourceOwner { _workspace: workspace, repository: repo,
        directory: HeldDir::root(&path, 0o700)?,
        git: HeldDir::root(&git_path, fs::symlink_metadata(&git_path)?.mode() & 0o7777)?,
        lock_id: identity(&lock)?, lock, lock_path, head: baseline };
    source.check()?; remaining(budget)?; Ok(source)
}
fn wait_actual(child: &mut Child, budget: &FinalizationBudget) -> Result<ExitStatus> {
    loop {
        remaining(budget)?; let status = child.try_wait()?; remaining(budget)?;
        if let Some(status) = status { return Ok(status); }
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn root_chown_fresh_child(path: &Path) -> Result<()> {
    let path = CString::new(path.as_os_str().as_bytes())?;
    syscall(unsafe { libc::chown(path.as_ptr(), READER_UID, READER_GID) }, "chown only new real-history child scratch")
}
fn erofs_head(root: &Path) -> Result<()> {
    let path = root.join("metadata/.git/HEAD"); let metadata = fs::symlink_metadata(&path)?;
    ensure!(metadata.is_file() && metadata.uid() == 1500 && metadata.gid() == 1500
        && metadata.mode() & 0o7777 == 0o600, "real seed HEAD must have writable-owner mode before RO probe");
    let before = fs::read(&path)?;
    let attempted = OpenOptions::new().append(true).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(&path);
    ensure!(attempted.as_ref().err().and_then(|e| e.raw_os_error()) == Some(libc::EROFS)
        && fs::read(path)? == before, "real seed HEAD append did not fail EROFS"); Ok(())
}

#[test]
#[ignore = "Root-only real-history physical fixture; exact filter on an owned disposable VM"]
fn root_seed_real_history_kernel_qualification() -> Result<()> {
    let original = FinalizationBudget::begin(Duration::from_secs(30))?;
    let cancelled = AtomicBool::new(false);
    ensure!(unsafe { libc::getuid() } == 0 && unsafe { libc::geteuid() } == 0
        && unsafe { libc::getgid() } == 0 && unsafe { libc::getegid() } == 0, "real-history fixture requires actual root");
    let work = Path::new("/run").join(format!("rudder-real-history-seed-{}", uuid::Uuid::new_v4()));
    fs::DirBuilder::new().mode(0o700).create(&work)?;
    let settings = fixture_settings()?;
    let source = pollster::block_on(make_source(&work, &settings, &original))?;
    let store = LocalStore::open(work.join("root-cas"))?;
    let empty = store.put_directory(DirectoryManifest { format_version: 1, mode: 0o755, entries: vec![] })?;
    let environment = store.put_metadata(ObjectKind::EnvironmentRoot, &EnvironmentV2 { format_version: 2,
        directory: empty, platform: BTreeMap::from([("fixture_abi".into(), "linux-real-history-seed".into())]),
        installer_inputs: vec![], provenance: BTreeMap::new() })?;
    store.pin("real-history-fixture-environment", &environment)?;
    let integration = work.join("private-integration"); fs::DirBuilder::new().mode(0o700).create(&integration)?;
    let export = work.join("seed-export"); let mut owner_checks = 0usize;
    let prepared = pollster::block_on(prepare_initial_source(store.clone(), InitialSourceRequest {
        source_git: &source.git.path, private_repository: &integration, seed_destination: &export,
        expected_head: &source.head.commit, environment: &environment, settings: &settings,
    }, &original, || { owner_checks += 1; source.check() }))?;
    ensure!(owner_checks > 0 && prepared.baseline.commit == source.head.commit
        && prepared.baseline.parents == source.head.parents && prepared.baseline.tree == source.head.tree,
        "real initial producer changed original Git history");
    ensure!(prepared.baseline.parents.len() == 1 && prepared.verified.receipt().tree == prepared.baseline.tree,
        "real initial producer lacks a verified parent/tree");
    let runnable = decode_runnable(&store.read_object(&prepared.runnable_root)?)?;
    let source_root = decode_source(&store.read_object(&prepared.source_root)?)?;
    ensure!(runnable.source == prepared.source_root && runnable.view_root == source_root.directory,
        "real source/runnable mapping differs");
    let seed = prepared.seed_root.clone();
    let mut owner = RootSeedMount::capture(store.clone(), seed.clone())?;
    // Original reader, child and actual source/prepared owners remain outside
    // the inner operation. No failed ACK/deadline or dropped Arc proves drain.
    let mut read = None; let mut child: Option<Child> = None; let mut reaped = false;
    let result = (|| -> Result<serde_json::Value> {
        source.check()?; owner.prepare(&original, &cancelled)?;
        read = Some(owner.acquire_read()?);
        let held = read.as_ref().unwrap(); held.template()?;
        let root = held.root().to_path_buf(); erofs_head(&root)?;
        ensure!(fs::read(root.join("metadata/.git/HEAD"))? == format!("{}\n", prepared.baseline.commit.0).as_bytes(),
            "sealed real Git HEAD differs from original accepted commit");
        ensure!(owner.cleanup_after_reader_drain(&original).is_err() && !owner.is_settled(), "real retained reader did not refuse cleanup");
        let child_root = work.join("child"); fs::DirBuilder::new().mode(0o700).create(&child_root)?;
        root_chown_fresh_child(&child_root)?; fs::set_permissions(&work, fs::Permissions::from_mode(0o711))?;
        let executable = std::env::current_exe()?.canonicalize()?;
        let child_test = format!("{}::root_seed_real_history_uid1500_child", module_path!().split_once("::").unwrap().1);
        child = Some(Command::new("/usr/bin/setpriv")
            .args(["--reuid=1500", "--regid=1500", "--clear-groups", "--inh-caps=-all", "--ambient-caps=-all", "--bounding-set=-all", "--no-new-privs", "--"])
            .arg(executable).args(["--exact", &child_test, "--ignored", "--test-threads=1"])
            .env_clear().env("RUDDER_REAL_SEED_ROOT", &root).env("RUDDER_REAL_SEED_CHILD", &child_root)
            .env("RUDDER_REAL_SEED_REF", String::from_utf8(canonical_json(&seed)?)?)
            .env("RUDDER_REAL_SEED_BASELINE", String::from_utf8(canonical_json(&prepared.baseline)?)?)
            .env("RUDDER_REAL_SEED_OPERATION", String::from_utf8(canonical_json(&prepared.operation)?)?)
            .env("RUDDER_REAL_SEED_PROFILE", String::from_utf8(canonical_json(&runnable.profile)?)?)
            .env("RUDDER_REAL_SEED_POLICY", String::from_utf8(canonical_json(&source_root.projection_policy)?)?)
            .env("RUDDER_REAL_SEED_MNT", fs::read_link("/proc/thread-self/ns/mnt")?)
            .env("RUDDER_REAL_SEED_BUDGET", String::from_utf8(canonical_json(&original.constraint())?)?)
            .current_dir(&child_root)
            .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?);
        let status = wait_actual(child.as_mut().unwrap(), &original)?; reaped = true;
        ensure!(status.success(), "actual real-history UID1500 child failed: {status}");
        source.check()?; held.check_current()?;
        let file = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(child_root.join("receipt.json"))?;
        let metadata = file.metadata()?;
        ensure!(metadata.is_file() && metadata.uid() == 1500 && metadata.gid() == 1500 && metadata.nlink() == 1
            && metadata.mode() & 0o7777 == 0o600 && metadata.len() <= 32768, "real-history child receipt invalid");
        let mut bytes = Vec::new(); file.take(32769).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 32768, "real-history child receipt exceeds bound");
        let proof: serde_json::Value = serde_json::from_slice(&bytes)?;
        ensure!(proof["seed"] == serde_json::to_value(&seed)? && proof["baseline"] == serde_json::to_value(&prepared.baseline)?
            && proof["operation"] == serde_json::to_value(&prepared.operation)? && proof["uid"] == 1500
            && proof["caps0_nnp1"] == true && proof["environment_cleared"] == true
            && proof["same_mount_namespace"] == true && proof["write_error"] == "EROFS"
            && proof["child_cas_owner"] == 1500 && proof["private_forks"] == 2 && proof["real_git_head"] == true
            && proof["real_parent_history"] == true && proof["first_fork_new_commit_operation"] == true
            && proof["second_fork_unchanged"] == true && proof["new_object_absent_from_second"] == true,
            "real-history child proof differs");
        let new_baseline: CommitIdentity = serde_json::from_value(proof["new_baseline"].clone())?;
        let new_operation: JjOperationId = serde_json::from_value(proof["new_operation"].clone())?;
        new_baseline.validate()?; new_operation.validate()?;
        ensure!(new_baseline.commit != prepared.baseline.commit && new_baseline.change != prepared.baseline.change
            && new_baseline.tree != prepared.baseline.tree && new_baseline.parents == vec![prepared.baseline.commit.clone()]
            && new_operation != prepared.operation, "real child did not report a distinct child commit/operation");
        ensure!(store.retention()?.pins.get(&owner.resources.source.name) == Some(&seed)
            && store.retention()?.pins.get(&prepared.seed_pin) == Some(&seed), "real source pins did not survive actual child reap");
        remaining(&original)?; Ok(proof)
    })();
    if child.is_some() && !reaped {
        let actual = child.as_mut().unwrap(); let _ = actual.kill();
        let recovery = FinalizationBudget::begin(Duration::from_secs(3))?;
        reaped = wait_actual(actual, &recovery).is_ok();
    }
    if child.is_some() && !reaped {
        eprintln!("real-history fixture recovery required: mount/pin retained; actual child reap unconfirmed");
        return Err(anyhow::anyhow!("owned disposable VM teardown required; real-history child remains unsettled"));
    }
    drop(read.take());
    let cleanup = FinalizationBudget::begin(Duration::from_secs(5))?;
    if result.is_ok() {
        let busy = open_dir(&owner.resources.target.join("objects"))?;
        let failure = owner.cleanup_after_reader_drain(&cleanup).expect_err("real kernel reader must provoke EBUSY");
        ensure!(failure.downcast_ref::<std::io::Error>().is_some_and(|e| e.raw_os_error() == Some(libc::EBUSY))
            && mount_at(&owner.resources.target)?.is_some() && !owner.is_settled()
            && store.retention()?.pins.get(&owner.resources.source.name) == Some(&seed), "real EBUSY discarded original mount/pin");
        drop(busy);
    }
    owner.cleanup_after_reader_drain(&cleanup)?;
    ensure!(owner.is_settled() && mount_at(&owner.resources.target)?.is_none()
        && !store.retention()?.pins.contains_key(&owner.resources.source.name), "real root mount cleanup unconfirmed");
    let proof = result?;
    source.check()?;
    // Only this fixture's known original pins are removed, AFTER child reap and
    // verified normal unmount. This does not pretend Journal adoption occurred.
    for (pin, reference) in [(prepared.runnable_pin.as_str(), &prepared.runnable_root), (prepared.seed_pin.as_str(), &seed),
        (prepared.verified.retention_pin(), prepared.verified.evidence()), ("real-history-fixture-environment", &environment)] {
        remaining(&cleanup)?;
        ensure!(store.retention()?.pins.get(pin) == Some(reference), "real fixture pin ownership changed");
        store.unpin(pin)?; remaining(&cleanup)?;
    }
    ensure!(store.retention()?.pins.is_empty(), "real fixture original pin cleanup incomplete");
    let baseline = prepared.baseline.clone(); let operation = prepared.operation.clone();
    drop(owner); drop(prepared); drop(source); drop(store);
    fs::remove_dir_all(&work)?; remaining(&cleanup)?;
    println!("{}", serde_json::json!({"scope":SCOPE,"seed":seed,"baseline":baseline,"operation":operation,
        "original_owner_checks":owner_checks,"real_source_history":true,"real_runnable_source_mapping":true,
        "root_write_error":"EROFS","read_owner_blocked_cleanup":true,"root_source_pin_retained_through_child":true,
        "actual_child_reaped":true,"actual_ebusy_retained_owner":true,"normal_unmount_verified":true,
        "own_mount_pin_removed":true,"fixture_original_pins_removed":true,"production_ipc":false,
        "spec3":false,"backend":false,"models":false,"child":proof}));
    Ok(())
}

async fn reopen(path: &Path, baseline: &CommitIdentity, operation: &JjOperationId, tracked: &[u8], parent_tracked: &[u8],
    budget: &FinalizationBudget) -> Result<Arc<ReadonlyRepo>> {
    remaining(budget)?; baseline.validate()?; operation.validate()?;
    let loader = RepoLoader::init_from_file_system(&fixture_settings()?, &path.join(".jj/repo"), &StoreFactories::default())?;
    let heads = loader.op_heads_store().get_op_heads().await?;
    ensure!(heads.len() == 1 && heads[0].hex() == operation.0, "real fork operation head differs");
    let repo = loader.load_at_head().await?;
    ensure!(repo.op_id().hex() == operation.0, "reopened real fork operation differs");
    let id = repo.view().get_wc_commit_id(WorkspaceName::DEFAULT).context("real fork WC commit absent")?;
    let commit = repo.store().get_commit(id)?;
    ensure!(CommitIdentity::from_jj(&commit) == *baseline
        && repo.view().get_local_bookmark(RefName::new(INTEGRATION_BOOKMARK)) == &RefTarget::normal(id.clone()),
        "real fork baseline/change/tree/bookmark differs");
    ensure!(baseline.parents.len() == 1, "real fork must preserve one actual parent");
    let parent_id = CommitId::try_from_hex(&baseline.parents[0].0).context("real parent ID invalid")?;
    let parent = repo.store().get_commit(&parent_id)?;
    ensure!(parent.id() == &parent_id, "real accepted parent missing");
    let backend = repo.store().backend_impl::<GitBackend>().context("real fork Git backend absent")?;
    let git = backend.git_repo();
    ensure!(git.head()?.id().context("real fork Git HEAD unborn")?.detach().to_string() == baseline.commit.0,
        "real fork Git HEAD differs from accepted baseline");
    // Pinned jj enables the raw object APIs, without gix's revision parser.
    let git_head = git.head()?.id().context("real fork Git HEAD absent")?.object()?.try_into_commit()?;
    let git_parent = git_head.parent_ids().next().context("real fork Git parent absent")?.object()?.try_into_commit()?;
    let accepted_blob = git_head.tree()?.lookup_entry_by_path("tracked")?.context("accepted Git path absent")?.object()?.try_into_blob()?.take_data();
    let parent_blob = git_parent.tree()?.lookup_entry_by_path("tracked")?.context("parent Git path absent")?.object()?.try_into_blob()?.take_data();
    ensure!(accepted_blob == tracked && parent_blob == parent_tracked, "real fork Git history preimages differ");
    remaining(budget)?; Ok(repo)
}

#[test]
#[ignore = "Only paired actual root fixture may invoke this UID1500 real-history child"]
fn root_seed_real_history_uid1500_child() -> Result<()> {
    ensure!(unsafe { libc::getuid() } == 1500 && unsafe { libc::geteuid() } == 1500
        && unsafe { libc::getgid() } == 1500 && unsafe { libc::getegid() } == 1500, "real-history child requires UID/GID1500");
    let status = String::from_utf8(proc_bytes("/proc/thread-self/status", 16384)?)?;
    for key in ["CapEff", "CapPrm", "CapInh", "CapAmb", "CapBnd"] {
        let prefix = format!("{key}:");
        let value = status.lines().find_map(|line| line.strip_prefix(prefix.as_str())).context("child capability missing")?.trim();
        ensure!(!value.is_empty() && value.bytes().all(|b| b == b'0'), "real-history child retained capabilities");
    }
    ensure!(status.lines().any(|l| l.strip_prefix("NoNewPrivs:").is_some_and(|v| v.trim() == "1"))
        && status.lines().any(|l| l.strip_prefix("Groups:").is_some_and(|v| v.trim().is_empty())), "real-history child NNP/groups differ");
    let mut keys: Vec<_> = std::env::vars_os().map(|(key, _)| key.to_string_lossy().into_owned()).collect(); keys.sort();
    let mut expected_keys = ["RUDDER_REAL_SEED_ROOT", "RUDDER_REAL_SEED_CHILD", "RUDDER_REAL_SEED_REF",
        "RUDDER_REAL_SEED_BASELINE", "RUDDER_REAL_SEED_OPERATION", "RUDDER_REAL_SEED_PROFILE", "RUDDER_REAL_SEED_POLICY",
        "RUDDER_REAL_SEED_MNT", "RUDDER_REAL_SEED_BUDGET"]; expected_keys.sort();
    ensure!(keys.iter().map(String::as_str).eq(expected_keys), "real-history child environment was not cleared");
    let root = PathBuf::from(std::env::var_os("RUDDER_REAL_SEED_ROOT").context("original real fixture root absent")?);
    let scratch = PathBuf::from(std::env::var_os("RUDDER_REAL_SEED_CHILD").context("original real fixture child scratch absent")?);
    let original = FinalizationBudget::from_constraint(&FinalizationConstraint::from_json(std::env::var("RUDDER_REAL_SEED_BUDGET")?.as_bytes())?)?;
    ensure!(fs::read_link("/proc/thread-self/ns/mnt")? == PathBuf::from(std::env::var_os("RUDDER_REAL_SEED_MNT").context("original real MNT absent")?), "real child mount namespace differs");
    let expected: ObjectRef = serde_json::from_str(&std::env::var("RUDDER_REAL_SEED_REF")?)?;
    let baseline: CommitIdentity = serde_json::from_str(&std::env::var("RUDDER_REAL_SEED_BASELINE")?)?;
    let operation: JjOperationId = serde_json::from_str(&std::env::var("RUDDER_REAL_SEED_OPERATION")?)?;
    let profile: ObjectRef = serde_json::from_str(&std::env::var("RUDDER_REAL_SEED_PROFILE")?)?;
    let policy: ObjectRef = serde_json::from_str(&std::env::var("RUDDER_REAL_SEED_POLICY")?)?;
    let profile_value = WorkspaceProfile::source_only();
    ensure!(profile == ObjectRef::from_bytes(ObjectKind::Blob, &canonical_json(&profile_value)?)
        && policy == SourceProjectionPolicy::from_import_policy(&profile_value.source_exclusions())?.reference()?, "real fixture profile/policy differs");
    remaining(&original)?;
    let store = LocalStore::open(scratch.join("independent-cas"))?;
    ensure!(fs::metadata(store.root())?.uid() == 1500, "real child CAS is not independently UID1500 owned");
    let seed = store.import_tree(&root, CheckpointConsistency::Quiesced)?;
    ensure!(seed == expected, "independent real child CAS changed exact seed identity");
    store.pin("real-child-original-seed", &seed)?;
    let verified = VerifiedMetadataTemplate::open_retained(&store, &seed, &root, &root.join("objects"))?;
    ensure!(verified.manifest().baseline == baseline && verified.manifest().operation == operation, "real retained manifest differs");
    erofs_head(&root)?;
    let binding = |id: &str| AttemptBinding { format_version: 1, project_id: "real-history-seed-physical-fixture".into(),
        owner: OwnerEpoch { owner: "root-fixture".into(), epoch: 1 }, run_id: id.into(), run_epoch: 1,
        attempt_id: id.into(), view_id: id.into(), seed: seed.clone(), baseline: baseline.clone(),
        baseline_operation: operation.clone(), projection_policy: policy.clone(), profile: profile.clone() };
    let first = scratch.join("fork-a"); let second = scratch.join("fork-b");
    for (id, path) in [("fork-a", &first), ("fork-b", &second)] {
        remaining(&original)?;
        let fork = fork_private_metadata(&verified, path, binding(id), None)?;
        ensure!(fork.kernel_readonly_objects && fs::read_to_string(path.join(".git/objects/info/alternates"))?
            == format!("{}\n", root.join("objects").display()), "real fork lost exact shared alternate path");
    }
    let (new_baseline, new_operation) = pollster::block_on(async {
        let repo = reopen(&first, &baseline, &operation, ACCEPTED_BYTES, PARENT_BYTES, &original).await?;
        let _second_before = reopen(&second, &baseline, &operation, ACCEPTED_BYTES, PARENT_BYTES, &original).await?;
        let baseline_id = CommitId::try_from_hex(&baseline.commit.0).context("real baseline ID invalid")?;
        let accepted = repo.store().get_commit(&baseline_id)?;
        let tree = changed_tree(&repo, accepted.tree(), MUTATED_BYTES, &original).await?;
        let mut tx = repo.start_transaction();
        let commit = tx.repo_mut().new_commit(vec![accepted.id().clone()], tree)
            .set_description("only first private real fork").clear_sign_key().write().await?;
        tx.repo_mut().set_wc_commit(WorkspaceName::DEFAULT.to_owned(), commit.id().clone())?;
        tx.repo_mut().set_local_bookmark_target(RefName::new(INTEGRATION_BOOKMARK), RefTarget::normal(commit.id().clone()));
        let changed = tx.commit("publish only first private real fork operation").await?;
        let new_baseline = CommitIdentity::from_jj(&commit); let new_operation = JjOperationId(changed.op_id().hex());
        ensure!(new_baseline.commit != baseline.commit && new_baseline.change != baseline.change
            && new_baseline.tree != baseline.tree && new_operation != operation, "first real private fork did not change");
        write_head(&first.join(".git/HEAD"), &new_baseline.commit)?;
        let _first_after = reopen(&first, &new_baseline, &new_operation, MUTATED_BYTES, ACCEPTED_BYTES, &original).await?;
        let second_after = reopen(&second, &baseline, &operation, ACCEPTED_BYTES, PARENT_BYTES, &original).await?;
        ensure!(second_after.store().get_commit(commit.id()).is_err(), "second real fork can see first fork's new private object");
        Ok::<_, anyhow::Error>((new_baseline, new_operation))
    })?;
    ensure!(fs::read(root.join("metadata/.git/HEAD"))? == format!("{}\n", baseline.commit.0).as_bytes(), "private commit changed sealed source metadata");
    remaining(&original)?;
    let receipt = canonical_json(&serde_json::json!({"seed":seed,"baseline":baseline,"operation":operation,
        "uid":1500,"caps0_nnp1":true,"environment_cleared":true,"same_mount_namespace":true,"write_error":"EROFS","child_cas_owner":1500,
        "private_forks":2,"real_git_head":true,"real_parent_history":true,"first_fork_new_commit_operation":true,
        "second_fork_unchanged":true,"new_object_absent_from_second":true,"new_baseline":new_baseline,"new_operation":new_operation}))?;
    ensure!(receipt.len() <= 32768, "real child receipt exceeds bound");
    let mut output = OpenOptions::new().write(true).create_new(true).mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(scratch.join("receipt.json"))?;
    output.write_all(&receipt)?; output.sync_all()?; remaining(&original)?; Ok(())
}
