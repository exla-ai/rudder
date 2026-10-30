use super::*;
use crate::store::{EntryNode, EnvironmentManifest, SourceManifest};
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
        let source_manifest: SourceManifest = serde_json::from_slice(&store.read_object(source)?)?;
        let environment: EnvironmentManifest =
            serde_json::from_slice(&store.read_object(&prepared.root)?)?;
        ensure!(
            environment.platform == profile.platform.properties(),
            "prepared environment and requested platform differ"
        );
        let preparation: PreparationSpec =
            decode_payload(store, &prepared.key, "prepared-environment-key-v1")?;
        preparation.validate()?;
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
        let result = (|| {
            let started = Instant::now();
            let prepared_path = materialize_environment(index, &environment.directory)?;
            let environment_materialization_ms = millis(started);
            let started = Instant::now();
            sync_inputs(store, &source_manifest.directory, &workspace)?;
            let source_materialization_ms = millis(started);
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
            let _execution = index.execution_guard(&action_key)?;
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
            let stored = index.record_build(&qualified)?;
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
            Ok(BuildRun {
                format_version: FORMAT_VERSION,
                stored,
                run_receipt: receipt,
                observation,
                frontend_graph_reused_actions: response.graph_reused_actions,
                frontend_elapsed_ms: response.elapsed_ms,
                environment_materialization_ms,
                source_materialization_ms,
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
        index.store().export_tree(root, &path)?;
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
    store.export_tree(root, &staging)?;
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
fn verify_directory(store: &LocalStore, root: &ObjectRef, path: &Path) -> Result<()> {
    let manifest = store.read_directory(root)?;
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
    let actual: BTreeSet<_> = fs::read_dir(path)?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<std::io::Result<_>>()?;
    let mut expected = BTreeSet::new();
    for entry in manifest.entries {
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
            EntryNode::Directory { object } => verify_directory(store, &object, &path)?,
            EntryNode::File { object, mode } => {
                let info = fs::symlink_metadata(&path)?;
                ensure!(
                    info.is_file()
                        && info.len() == object.size_bytes
                        && hash_file(&path)? == object.digest,
                    "materialized tool/source file changed"
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
    ensure!(
        actual == expected,
        "materialized input directory has undeclared entries"
    );
    Ok(())
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
