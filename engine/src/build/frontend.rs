use super::*;
use crate::directory::EntryNode;
use fs2::FileExt;
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use uuid::Uuid;

const DRIVER: &str = include_str!("frontend_driver.py");
const RUNNER: &str = include_str!("linux_runner.py");

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildRun {
    pub format_version: u32,
    pub stored: StoredBuild,
    pub run_receipt: ObjectRef,
    pub observation: FrontendObservation,
    pub frontend_graph_reused_actions: u64,
    pub frontend_elapsed_ms: u64,
    pub environment_materialization_ms: u64,
    pub source_materialization_ms: u64,
    /// Inclusive local stage wall times, separate from Buck2's own elapsed
    /// counter. This exposes validation/publication overhead on cache hits.
    #[serde(default)]
    pub stage_ms: BTreeMap<String, u64>,
}

pub struct Buck2Adapter {
    executable: PathBuf,
}
impl Buck2Adapter {
    pub fn new(executable: impl AsRef<Path>) -> Result<Self> {
        ensure!(
            cfg!(target_os = "linux"),
            "managed Buck2 compile profile requires Linux"
        );
        let executable = executable.as_ref().canonicalize()?;
        ensure!(
            fs::metadata(&executable)?.is_file(),
            "Buck2 executable is not a file"
        );
        Ok(Self { executable })
    }

    pub fn stop(&self, workspace: impl AsRef<Path>) -> Result<()> {
        let workspace = workspace.as_ref().canonicalize()?;
        ensure!(
            workspace.join(".rudder-managed-build.json").is_file(),
            "refusing to stop an unmanaged Buck2 project"
        );
        let _lock = lock(&workspace.join(".rudder-build-lock"), false)?;
        let mut command = Command::new(&self.executable);
        command
            .arg("kill")
            .current_dir(&workspace)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", workspace.join(".driver/home"));
        bounded(
            &mut command,
            Duration::from_secs(15),
            &workspace.join(".stop.stdout"),
            &workspace.join(".stop.stderr"),
        )
    }

    pub fn build(
        &self,
        index: &BuildIndex,
        profile: &BuildProfile,
        input: &BuildInputView,
        prepared: &PreparedEnvironment,
        workspace: impl AsRef<Path>,
    ) -> Result<BuildRun> {
        let build_started = Instant::now();
        let mut stage_ms = BTreeMap::new();
        profile.validate()?;
        ensure!(
            input.hardlink_group_count() == 0,
            "this frontend needs a hardlink-preserving VFS input adapter before building linked checkpoint inputs"
        );
        ensure!(
            profile.policy.cacheable(),
            "secret, deployment and nonhermetic steps are outside the reusable compiler profile"
        );
        ensure!(
            !input.dirty_prepared(),
            "dirty prepared inputs require re-preparation and a new environment identity"
        );
        ensure!(
            input.base_environment() == Some(&prepared.root),
            "build checkpoint environment differs from the prepared index root"
        );
        let source = input.source_root();
        ensure!(
            profile.platform.architecture == std::env::consts::ARCH,
            "profile architecture differs from the running compiler host"
        );
        ensure!(
            hash_file(&self.executable)? == profile.buck2.executable_sha256,
            "Buck2 executable checksum does not match the profile"
        );
        index.validate_prepared(prepared)?;
        let store = index.store();
        kind(source, ObjectKind::SourceRoot)?;
        let source_directory = source_directory(store, source)?;
        let environment = environment_content(store, &prepared.root)?;
        ensure!(
            environment.platform == profile.platform.properties(),
            "prepared environment and requested platform differ"
        );
        let preparation = index.preparation_spec(prepared)?;
        stage_ms.insert("preflight".into(), millis(build_started));
        let workspace_started = Instant::now();
        let workspace = workspace.as_ref();
        fs::create_dir_all(workspace)?;
        ensure!(
            fs::symlink_metadata(workspace)?.is_dir(),
            "build workspace is not a real directory"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(workspace, fs::Permissions::from_mode(0o700))?;
        }
        let workspace = workspace.canonicalize()?;
        let _workspace_lock = lock(&workspace.join(".rudder-build-lock"), false)?;
        let marker = workspace.join(".rudder-managed-build.json");
        if marker.exists() {
            let value: Value = serde_json::from_slice(&fs::read(&marker)?)?;
            ensure!(
                value["format_version"] == FORMAT_VERSION && value["profile"] == profile.name,
                "build directory belongs to another profile"
            );
        } else {
            ensure!(
                fs::read_dir(&workspace)?
                    .filter_map(std::result::Result::ok)
                    .all(|entry| entry.file_name() == ".rudder-build-lock"),
                "refusing to overwrite an unmanaged build directory"
            );
            atomic(
                &marker,
                &canonical_json(
                    &serde_json::json!({"format_version":FORMAT_VERSION,"profile":profile.name}),
                )?,
            )?;
        }
        let inflight = workspace.join(".rudder-build-inflight");
        if inflight.exists() {
            let mut kill = Command::new(&self.executable);
            kill.arg("kill")
                .current_dir(&workspace)
                .env_clear()
                .env("PATH", "/usr/bin:/bin");
            bounded(
                &mut kill,
                Duration::from_secs(15),
                &workspace.join(".recovery-kill.stdout"),
                &workspace.join(".recovery-kill.stderr"),
            )?;
            fs::remove_file(&inflight)?;
        }
        let lease = store.begin_staging(Duration::from_secs(3600))?;
        stage_ms.insert("workspace_setup".into(), millis(workspace_started));
        let result = (|| {
            let started = Instant::now();
            let prepared_path = materialize_environment(index, &environment.directory)?;
            let environment_materialization_ms = millis(started);
            stage_ms.insert(
                "environment_materialization".into(),
                environment_materialization_ms,
            );
            let started = Instant::now();
            sync_inputs(store, &source_directory, &workspace)?;
            let source_materialization_ms = millis(started);
            stage_ms.insert("source_materialization".into(), source_materialization_ms);
            let contract_started = Instant::now();
            for target in &profile.targets {
                for path in &target.sources {
                    let full = workspace.join("inputs").join(path);
                    ensure!(
                        fs::symlink_metadata(full)?.is_file(),
                        "managed compiler sources must be ordinary files"
                    );
                }
            }
            atomic(&workspace.join("frontend_driver.py"), DRIVER.as_bytes())?;
            atomic(&workspace.join("linux_runner.py"), RUNNER.as_bytes())?;
            let rule_bytes = canonical_json(
                &serde_json::json!({"driver":DRIVER,"runner":RUNNER,"schema":"rudder-linux-compile-v1","targets":profile.targets}),
            )?;
            let rules = store.put_blob(&rule_bytes)?;
            let frontend_executable = store.put_blob(&fs::read(&self.executable)?)?;
            let mut platform = profile.platform.clone();
            platform.properties.insert(
                "kernel_release".into(),
                fs::read_to_string("/proc/sys/kernel/osrelease")?
                    .trim()
                    .into(),
            );
            let contract = ActionContract {
                format_version: FORMAT_VERSION,
                frontend: "buck2-rudder-linux-compile-v1".into(),
                frontend_executable: frontend_executable.clone(),
                rules: rules.clone(),
                source_root: source.clone(),
                input_topology: input.hardlinks().cloned(),
                environment_root: prepared.root.clone(),
                platform: platform.clone(),
                toolchains: preparation.toolchains.clone(),
                command: vec!["buck2".to_owned(), "build".to_owned()]
                    .into_iter()
                    .chain(
                        profile
                            .targets
                            .iter()
                            .map(|target| format!("//:{}", target.name)),
                    )
                    .collect(),
                environment: profile.policy.environment.clone(),
                outputs: profile
                    .targets
                    .iter()
                    .map(|target| target.name.clone())
                    .collect(),
                policy: profile.policy.clone(),
            };
            // A complete-contract miss is coalesced before invoking the
            // frontend. A waiting fresh graph can then obtain native REAPI
            // hits from the first builder's completed, verified uploads.
            let action_key = contract.key(store)?;
            stage_ms.insert("contract".into(), millis(contract_started));
            let guard_started = Instant::now();
            let _execution = index.execution_guard(&action_key)?;
            stage_ms.insert("execution_guard_wait".into(), millis(guard_started));
            let frontend_started = Instant::now();
            let native_contract = serde_json::json!({"format_version":FORMAT_VERSION,"environment":prepared.root,"toolchains":preparation.toolchains,"platform":platform,"policy":profile.policy,"rules":rules,"frontend":frontend_executable,"input_topology":input.hardlinks()});
            let request = serde_json::json!({"format_version":FORMAT_VERSION,"workspace":workspace,"prepared_directory":prepared_path,"buck2":self.executable,"profile":profile,"native_contract":native_contract});
            let request_path = workspace.join(".driver-request.json");
            atomic(&request_path, &canonical_json(&request)?)?;
            atomic(
                &inflight,
                &canonical_json(
                    &serde_json::json!({"source":source,"environment":prepared.root,"attempt":Uuid::new_v4().to_string()}),
                )?,
            )?;
            let stdout = workspace.join(".driver-response.json");
            let stderr = workspace.join(".driver-error.log");
            let mut command = Command::new("/usr/bin/python3");
            command
                .args(["-I", "frontend_driver.py"])
                .current_dir(&workspace)
                .env_clear()
                .env("PATH", "/usr/local/bin:/usr/bin:/bin")
                .stdin(File::open(&request_path)?);
            let outcome = bounded(&mut command, Duration::from_secs(150), &stdout, &stderr);
            if outcome.is_ok() {
                fs::remove_file(&inflight)?;
            }
            outcome.with_context(|| {
                format!(
                    "managed Buck2 frontend failed: {}",
                    read_tail(&stderr, 8000).unwrap_or_default()
                )
            })?;
            stage_ms.insert("frontend_dispatch".into(), millis(frontend_started));
            let capture_started = Instant::now();
            // Never reacquire an expired lease over possibly swept inputs.
            // Renew only a still-live lease before creating result objects.
            store.renew_staging(&lease.id, Duration::from_secs(3600))?;
            #[derive(Deserialize)]
            struct Response {
                format_version: u32,
                trace_id: String,
                executed_actions: u64,
                remote_cache_hits: u64,
                graph_reused_actions: u64,
                native_action_digests: Vec<ReapiDigest>,
                outputs: BTreeMap<String, String>,
                event_log: PathBuf,
                build_report: PathBuf,
                elapsed_ms: u64,
                sandbox_qualification: Value,
            }
            let response: Response = serde_json::from_slice(&fs::read(&stdout)?)?;
            ensure!(
                response.format_version == FORMAT_VERSION
                    && response.sandbox_qualification
                        == serde_json::json!({"host_files_visible":false,"host_network_reachable":false}),
                "compiler sandbox did not qualify"
            );
            // Each managed target has one compile action. Missing trace records
            // cannot be interpreted as remote-cache evidence.
            ensure!(
                response.executed_actions
                    + response.remote_cache_hits
                    + response.graph_reused_actions
                    == profile.targets.len() as u64,
                "inconsistent frontend action accounting"
            );
            let output_dir = workspace.join(format!(".outputs-{}", Uuid::new_v4()));
            fs::create_dir(&output_dir)?;
            for target in &profile.targets {
                let path = response
                    .outputs
                    .get(&target.name)
                    .context("frontend omitted target output")?;
                relative(path)?;
                let actual = workspace.join(path).canonicalize()?;
                ensure!(
                    actual.starts_with(workspace.join("buck-out")),
                    "frontend output escaped managed scratch"
                );
                let destination = output_dir.join(&target.name);
                if actual.is_dir() {
                    copy_tree(&actual, &destination)?;
                } else {
                    fs::create_dir(&destination)?;
                    fs::copy(
                        &actual,
                        destination.join(actual.file_name().context("output name missing")?),
                    )?;
                }
            }
            let outputs = store.import_tree(
                &output_dir,
                crate::protocol::CheckpointConsistency::Quiesced,
            )?;
            fs::remove_dir_all(&output_dir)?;
            let event_log = store.put_blob(&fs::read(checked_raw_path(
                &workspace,
                &response.event_log,
            )?)?)?;
            let build_report = store.put_blob(&fs::read(checked_raw_path(
                &workspace,
                &response.build_report,
            )?)?)?;
            let observation = FrontendObservation {
                frontend: "buck2-rudder-linux-compile-v1".into(),
                trace_id: response.trace_id,
                executed_actions: response.executed_actions,
                remote_cache_hits: response.remote_cache_hits,
                native_action_digests: response.native_action_digests,
                event_log,
                build_report,
            };
            let qualified = QualifiedBuild {
                contract,
                prepared: prepared.clone(),
                outputs,
                observation: observation.clone(),
            };
            stage_ms.insert("result_capture".into(), millis(capture_started));
            let index_started = Instant::now();
            let stored = index.record_build(&qualified)?;
            stage_ms.insert("action_index".into(), millis(index_started));
            let publication_started = Instant::now();
            let receipt = metadata(
                store,
                ObjectKind::ActionResult,
                "managed-build-run-v1",
                &serde_json::json!({"stored":stored.receipt,"observation":observation,"input_view":input,"frontend_graph_reused_actions":response.graph_reused_actions,"frontend_elapsed_ms":response.elapsed_ms}),
                vec![
                    stored.receipt.clone(),
                    observation.event_log.clone(),
                    observation.build_report.clone(),
                    input.checkpoint().clone(),
                ],
            )?;
            store.publish_extending(
                &lease.id,
                &format!("build/run/{}", observation.trace_id),
                &receipt,
                &[prepared.receipt.clone(), stored.receipt.clone()],
            )?;
            stage_ms.insert(
                "run_receipt_publication".into(),
                millis(publication_started),
            );
            stage_ms.insert("build_total".into(), millis(build_started));
            Ok(BuildRun {
                format_version: FORMAT_VERSION,
                stored,
                run_receipt: receipt,
                observation,
                frontend_graph_reused_actions: response.graph_reused_actions,
                frontend_elapsed_ms: response.elapsed_ms,
                environment_materialization_ms,
                source_materialization_ms,
                stage_ms,
            })
        })();
        if result.is_err() {
            let _ = store.abandon_staging(&lease.id);
        }
        result
    }
}

fn materialize_environment(index: &BuildIndex, root: &ObjectRef) -> Result<PathBuf> {
    let base = index.root().join("materialized");
    fs::create_dir_all(&base)?;
    let path = base.join(&root.digest);
    let marker = base.join(format!("{}.complete", root.digest));
    let _lock = lock(&base.join(format!("{}.lock", root.digest)), marker.exists())?;
    if !marker.exists() {
        if path.exists() {
            fs::remove_dir_all(&path)?;
        }
        export_unlinked_tree(index.store(), root, &path)?;
        atomic(&marker, &canonical_json(root)?)?;
    }
    ensure!(
        serde_json::from_slice::<ObjectRef>(&fs::read(marker)?)? == *root,
        "materialized environment identity mismatch"
    );
    verify_directory(index.store(), root, &path)?;
    Ok(path)
}
fn sync_inputs(store: &LocalStore, root: &ObjectRef, workspace: &Path) -> Result<()> {
    let path = workspace.join("inputs");
    let marker = workspace.join(".inputs-root.json");
    if marker.exists() && serde_json::from_slice::<ObjectRef>(&fs::read(&marker)?)? == *root {
        return verify_directory(store, root, &path);
    }
    let staging = workspace.join(format!(".inputs-{}", Uuid::new_v4()));
    export_unlinked_tree(store, root, &staging)?;
    // Apply the same input contract on first use and graph reuse. Otherwise
    // an unused source symlink could pass cold and fail only on the next run.
    verify_directory(store, root, &staging)?;
    if path.exists() {
        fs::remove_dir_all(&path)?;
    }
    fs::rename(&staging, &path)?;
    atomic(&marker, &canonical_json(root)?)?;
    Ok(())
}
fn export_unlinked_tree(store: &LocalStore, root: &ObjectRef, path: &Path) -> Result<()> {
    // This frontend still rejects linked build inputs. Environment identity is
    // file content, without inode topology. Export uses bounded chunk streams
    // and fresh private files for either directory encoding.
    let hardlinks = store.put_blob(&canonical_json(&crate::tree::HardlinkTopology {
        format_version: 1,
        groups: vec![],
    })?)?;
    crate::tree::export_quiesced(
        store,
        &crate::tree::TreeSnapshot {
            format_version: 1,
            directory: root.clone(),
            hardlinks,
        },
        path,
        &crate::tree::ImportBudget::default(),
    )
}
fn verify_directory(store: &LocalStore, root: &ObjectRef, path: &Path) -> Result<()> {
    let mut entries = 0;
    let mut logical_bytes = 0;
    verify_directory_bounded(store, root, path, 0, &mut entries, &mut logical_bytes)
}
fn verify_directory_bounded(
    store: &LocalStore,
    root: &ObjectRef,
    path: &Path,
    depth: usize,
    entries: &mut u64,
    logical_bytes: &mut u64,
) -> Result<()> {
    let budget = crate::tree::ImportBudget::default();
    ensure!(
        depth <= budget.max_depth,
        "materialized input exceeds depth budget"
    );
    let manifest = crate::tree::root_info(store, root)?;
    *entries = entries
        .checked_add(manifest.entry_count)
        .context("materialized input entry count overflow")?;
    ensure!(
        *entries <= budget.max_entries,
        "materialized input exceeds entry budget"
    );
    let directory_info = fs::symlink_metadata(path)?;
    ensure!(
        directory_info.is_dir(),
        "materialized directory is missing or replaced"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            directory_info.permissions().mode() & 0o777 == manifest.mode,
            "materialized directory mode changed"
        );
    }
    let mut actual = BTreeSet::new();
    for entry in fs::read_dir(path)? {
        ensure!(
            actual.len() < budget.max_entries as usize,
            "materialized directory exceeds entry budget"
        );
        actual.insert(entry?.file_name());
    }
    let mut expected = BTreeSet::new();
    let mut after = None;
    loop {
        let page = crate::tree::scan(store, root, after.as_deref(), 1024)?;
        for entry in page.entries {
            let name = entry.name()?;
            #[cfg(unix)]
            let name = {
                use std::os::unix::ffi::OsStringExt;
                std::ffi::OsString::from_vec(name)
            };
            #[cfg(not(unix))]
            let name = std::ffi::OsString::from(String::from_utf8(name)?);
            expected.insert(name.clone());
            let path = path.join(name);
            match entry.node {
                EntryNode::Directory { object } => verify_directory_bounded(
                    store,
                    &object,
                    &path,
                    depth + 1,
                    entries,
                    logical_bytes,
                )?,
                EntryNode::File {
                    object,
                    logical_size,
                    mode,
                } => {
                    *logical_bytes = logical_bytes
                        .checked_add(logical_size)
                        .context("materialized input length overflow")?;
                    ensure!(
                        *logical_bytes <= budget.max_logical_bytes,
                        "materialized input exceeds logical byte budget"
                    );
                    let info = fs::symlink_metadata(&path)?;
                    ensure!(
                        info.is_file() && info.len() == logical_size,
                        "materialized tool/source file changed"
                    );
                    let mut options = OpenOptions::new();
                    options.read(true);
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::OpenOptionsExt;
                        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
                    }
                    let mut actual = options.open(&path)?;
                    let mut compare = FileComparison {
                        actual: &mut actual,
                    };
                    ensure!(
                        crate::content::export_file(store, &object, &mut compare)?.logical_size
                            == logical_size,
                        "materialized content logical size differs"
                    );
                    ensure!(
                        actual.read(&mut [0u8; 1])? == 0,
                        "materialized file has trailing data"
                    );
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        ensure!(
                            info.permissions().mode() & 0o777 == mode,
                            "materialized file mode changed"
                        );
                    }
                }
                EntryNode::Symlink { .. } => {
                    anyhow::bail!("managed compiler profile does not admit symlink inputs")
                }
            }
        }
        after = page.next_after;
        if after.is_none() {
            break;
        }
    }
    ensure!(
        actual == expected,
        "materialized input directory has undeclared entries"
    );
    Ok(())
}

struct FileComparison<'a> {
    actual: &'a mut File,
}
impl Write for FileComparison<'_> {
    fn write(&mut self, expected: &[u8]) -> std::io::Result<usize> {
        let mut buffer = [0u8; 64 * 1024];
        for chunk in expected.chunks(buffer.len()) {
            self.actual.read_exact(&mut buffer[..chunk.len()])?;
            if buffer[..chunk.len()] != *chunk {
                return Err(std::io::Error::other(
                    "materialized tool/source bytes changed",
                ));
            }
        }
        Ok(expected.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn hash_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}
fn atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let temporary = path.with_extension(format!("tmp-{}", Uuid::new_v4()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&temporary, path)?;
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}
fn lock(path: &Path, shared: bool) -> Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let file = options.open(path)?;
    if shared {
        FileExt::lock_shared(&file)?;
    } else {
        FileExt::lock_exclusive(&file)?;
    }
    Ok(file)
}
fn millis(started: Instant) -> u64 {
    started.elapsed().as_millis().try_into().unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::{self, StagedContentStore};
    use crate::directory::{DirectoryEntry, LocalDirectoryStore, PagedDirectory};
    use std::io::{Seek, SeekFrom};

    fn content_tree(store: &LocalStore) -> (ObjectRef, ObjectRef, usize) {
        let lease = store.begin_staging(Duration::from_secs(60)).unwrap();
        let bytes = vec![b'p'; content::CHUNK_BYTES + 37];
        let file = content::import_file(
            &StagedContentStore::new(store, &lease.id).unwrap(),
            &mut bytes.as_slice(),
        )
        .unwrap();
        assert_eq!(file.kind, ObjectKind::File);
        assert_ne!(file.digest, hex::encode(Sha256::digest(&bytes)));
        let adapter = LocalDirectoryStore::new(store, &lease.id);
        let directory = PagedDirectory::new(&adapter)
            .build(
                0o755,
                vec![DirectoryEntry::new(
                    b"input",
                    EntryNode::File {
                        object: file.clone(),
                        logical_size: bytes.len() as u64,
                        mode: 0o640,
                    },
                )
                .unwrap()],
            )
            .unwrap();
        store
            .publish(&lease.id, "frontend-chunked-fixture", &directory)
            .unwrap();
        (directory, file, bytes.len())
    }

    #[test]
    fn paged_chunked_inputs_materialize_and_changed_tail_invalidates_reuse() {
        let tmp = tempfile::tempdir().unwrap();
        let store = LocalStore::open(tmp.path().join("cas")).unwrap();
        let (directory, _, logical_size) = content_tree(&store);
        let workspace = tmp.path().join("build");
        fs::create_dir(&workspace).unwrap();
        sync_inputs(&store, &directory, &workspace).unwrap();
        let input = workspace.join("inputs/input");
        assert_eq!(fs::metadata(&input).unwrap().len(), logical_size as u64);
        sync_inputs(&store, &directory, &workspace).unwrap();
        let mut changed = OpenOptions::new().write(true).open(&input).unwrap();
        changed
            .seek(SeekFrom::Start(content::CHUNK_BYTES as u64 + 2))
            .unwrap();
        changed.write_all(b"x").unwrap();
        changed.sync_all().unwrap();
        assert!(
            sync_inputs(&store, &directory, &workspace).is_err(),
            "matching root marker cannot authorize changed chunked bytes"
        );
    }

    #[test]
    fn paged_environment_materialization_rechecks_content_and_rejects_added_files() {
        let tmp = tempfile::tempdir().unwrap();
        let store = LocalStore::open(tmp.path().join("cas")).unwrap();
        let (directory, _, _) = content_tree(&store);
        let index = BuildIndex::open(tmp.path().join("index"), store).unwrap();
        let path = materialize_environment(&index, &directory).unwrap();
        assert_eq!(materialize_environment(&index, &directory).unwrap(), path);
        fs::write(path.join("unmodeled"), b"new input").unwrap();
        assert!(materialize_environment(&index, &directory).is_err());
    }
}
fn bounded(command: &mut Command, timeout: Duration, stdout: &Path, stderr: &Path) -> Result<()> {
    command
        .stdout(Stdio::from(File::create(stdout)?))
        .stderr(Stdio::from(File::create(stderr)?));
    let mut child = command.spawn()?;
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            ensure!(status.success(), "build subprocess failed: {status}");
            return Ok(());
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("build subprocess exceeded its deadline");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn checked_raw_path(workspace: &Path, path: &Path) -> Result<PathBuf> {
    let path = path.canonicalize()?;
    ensure!(
        path.starts_with(workspace.join(".driver")),
        "frontend evidence escaped managed raw-log directory"
    );
    Ok(path)
}
fn read_tail(path: &Path, limit: usize) -> Result<String> {
    let bytes = fs::read(path)?;
    Ok(String::from_utf8_lossy(&bytes[bytes.len().saturating_sub(limit)..]).into())
}
fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    ensure!(
        fs::symlink_metadata(source)?.is_dir(),
        "compiler output directory is a symlink"
    );
    fs::create_dir(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let output = destination.join(entry.file_name());
        if kind.is_dir() {
            copy_tree(&entry.path(), &output)?;
        } else {
            ensure!(
                kind.is_file(),
                "compiler produced an unsupported output type"
            );
            fs::copy(entry.path(), output)?;
        }
    }
    Ok(())
}
