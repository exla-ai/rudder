#[path = "authority.rs"]
mod authority;
#[path = "guest.rs"]
mod guest;
#[path = "native_driver.rs"]
mod native_driver;
#[path = "services.rs"]
mod services;
#[path = "wire.rs"]
mod wire;

use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use jj_lib::git_backend::GitBackend;
use jj_lib::merged_tree::MergedTree;
use jj_lib::object_id::ObjectId;
use jj_lib::ref_name::WorkspaceName;
use jj_lib::repo::Repo;
use jj_lib::workspace::Workspace;
use rudder_engine::content::{self, StagedContentStore};
use rudder_engine::directory::{DirectoryEntry, EntryNode, LocalDirectoryStore, PagedDirectory};
use rudder_engine::execution::{
    BrokerBinding, ExecutionSpec, ReadOnlyRuntime, WorkspaceObservationProfile,
    handoff::ExecutionHandoff,
};
use rudder_engine::protocol::{OwnerEpoch, canonical_json};
use rudder_engine::store::LocalStore;
use rudder_engine::vfs::rpc::{AttemptContext, ObservationBinding};
use rudder_engine::vfs::{VfsManager, exclusion_policy_digest};
use rudder_engine::workspace::WorkspaceProfile;
use rudder_vcs_bridge::command_transport::{CommandReceipt, CommandServiceConfig, CommandState};
use rudder_vcs_bridge::projection::project_source;
use rudder_vcs_bridge::projection_io::ProjectionBudget;
use rudder_vcs_bridge::rpc_view::RpcViewConfig;
use rudder_vcs_bridge::vfs_provider::SourceProjectionPolicy;
use rudder_vcs_bridge::{AttemptBinding, CommitIdentity, JjOperationId};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::{fs::OpenOptionsExt, process::CommandExt};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{atomic::Ordering, mpsc};
use std::thread;
use std::time::{Duration, Instant};
use uuid::Uuid;

pub fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|arg| arg.starts_with("native-")) {
        return native_driver::main(&args);
    }
    if args
        .iter()
        .map(String::as_str)
        .eq(["fabric", "execution", "child"])
    {
        return rudder_engine::execution::linux::run_child();
    }
    if args.first().map(String::as_str) == Some("fabric") {
        ensure!(
            args.len() == 7
                && args[1] == "execution"
                && args[2] == "helper"
                && args[3] == "--handoff"
                && args[5] == "--parent-pid",
            "unsupported internal helper arguments"
        );
        let spec = rudder_engine::execution::handoff::receive(&args[4], args[6].parse()?)?;
        let outcome =
            rudder_engine::execution::run_helper_with_services(spec, services::factory())?;
        println!(
            "{}",
            serde_json::json!({"kind":"helper-proof","isolation":outcome.isolation,"warnings":outcome.warnings})
        );
        std::process::exit(outcome.record.agent_exit.unwrap_or(125));
    }
    match args.first().map(String::as_str) {
        Some("as-user") => {
            ensure!(
                args.len() == 3 && unsafe { libc::geteuid() } == 0,
                "requires owned VM root bootstrap"
            );
            unsafe {
                ensure!(
                    libc::setgroups(0, std::ptr::null()) == 0
                        && libc::setgid(1500) == 0
                        && libc::setuid(1500) == 0,
                    "cannot enter qualification UID/GID1500"
                );
            }
            bail!(
                "fresh UID exec failed: {}",
                Command::new(std::env::current_exe()?)
                    .args(["qualify", &args[1], &args[2]])
                    .exec()
            );
        }
        Some("qualify") => {
            ensure!(args.len() == 3, "qualify JJ_BINARY DATA_ROOT");
            qualify(Path::new(&args[1]), Path::new(&args[2]))
        }
        Some("guest") => guest::run(args.get(1).context("guest case absent")?),
        Some("writer") => guest::writer(),
        _ => bail!("explicit qualification mode required"),
    }
}

struct Fixture {
    spec: ExecutionSpec,
    config: services::Config,
    authority: Option<authority::Server>,
    store: LocalStore,
}
async fn fixture(root: &Path, binary: &Path, mode: &str) -> Result<Fixture> {
    wire::private(root)?;
    for name in [
        "authority",
        "private-jj",
        "projection",
        "command-state",
        "command-endpoint",
        "control",
        "broker",
    ] {
        wire::private(&root.join(name))?;
    }
    let store = LocalStore::open(root.join("cas"))?;
    let stage = store.begin_staging(Duration::from_secs(600))?;
    let content = StagedContentStore::new(&store, &stage.id)?;
    let bytes = b"export const n = 1;\n";
    let object = content::import_file(&content, &mut bytes.as_slice())?;
    let source = PagedDirectory::new(&LocalDirectoryStore::new(&store, &stage.id)).build(
        0o755,
        [DirectoryEntry::new(
            b"source.ts",
            EntryNode::File {
                object,
                logical_size: bytes.len() as u64,
                mode: 0o644,
            },
        )?],
    )?;
    store.publish(&stage.id, "fixture-source", &source)?;
    let profile = WorkspaceProfile::source_only();
    let policy = SourceProjectionPolicy::from_import_policy(&profile.source_exclusions())?;
    let profile_ref = store.put_blob(&canonical_json(&profile)?)?;
    let policy_ref = store.put_blob(&canonical_json(&policy)?)?;
    store.pin("fixture-profile", &profile_ref)?;
    store.pin("fixture-policy", &policy_ref)?;
    let metadata = root.join("private-jj");
    let (_wc, repo) = Workspace::init_internal_git(&services::settings(), &metadata).await?;
    let empty = MergedTree::resolved(repo.store().clone(), repo.store().empty_tree_id().clone());
    let projected = project_source(
        store.clone(),
        &source,
        &empty,
        &policy_ref,
        None,
        None,
        ProjectionBudget::default(),
    )
    .await?;
    let baseline = repo.store().get_commit(
        repo.view()
            .get_wc_commit_id(WorkspaceName::DEFAULT)
            .context("initial WC absent")?,
    )?;
    let mut tx = repo.start_transaction();
    let baseline = tx
        .repo_mut()
        .rewrite_commit(&baseline)
        .set_tree(projected.tree)
        .write()
        .await?;
    tx.repo_mut()
        .set_wc_commit(WorkspaceName::DEFAULT.to_owned(), baseline.id().clone())?;
    tx.repo_mut().rebase_descendants().await?;
    let repo = tx.commit("prepared nonempty mounted VCS fixture").await?;
    let private_git = repo
        .store()
        .backend_impl::<GitBackend>()
        .context("not Git backend")?
        .git_repo_path()
        .canonicalize()?;
    let hooks = private_git.join("hooks");
    if hooks.exists() {
        fs::remove_dir_all(hooks)?;
    }
    let owner = OwnerEpoch {
        owner: "fixture-outer-engine".into(),
        epoch: 1,
    };
    let view_id = Uuid::new_v4().to_string();
    let manager = VfsManager::open(root.join("vfs"), store.clone())?;
    manager.prepare_paged(&source)?;
    let view = manager.fork_paged(&view_id, &source, owner.clone())?;
    let cursor = view.dirty_cursor()?;
    let observation = ObservationBinding {
        format_version: 1,
        attempt: AttemptContext {
            execution_id: Uuid::new_v4().to_string(),
            operation_id: Uuid::new_v4().to_string(),
        },
        view_id: view_id.clone(),
        owner: owner.clone(),
        namespace: cursor.namespace,
        profile: profile_ref.clone(),
        exclusion_policy_digest: exclusion_policy_digest(&profile.source_exclusions())?,
    };
    drop(view);
    let attempt = AttemptBinding {
        format_version: 1,
        project_id: "mounted-fixture".into(),
        owner: owner.clone(),
        run_id: format!("mounted-{mode}"),
        run_epoch: 1,
        attempt_id: Uuid::new_v4().to_string(),
        view_id: view_id.clone(),
        seed: source,
        baseline: CommitIdentity::from_jj(&baseline),
        baseline_operation: JjOperationId(repo.op_id().hex()),
        projection_policy: policy_ref,
        profile: profile_ref,
    };
    let authority = authority::Server::bind(
        &root.join("authority/control.sock"),
        observation.clone(),
        attempt.clone(),
    )?;
    let public_command_endpoint =
        Path::new("/opt/runtime/vcs-endpoints").join(Uuid::new_v4().simple().to_string());
    wire::private(&public_command_endpoint)?;
    let view = RpcViewConfig {
        format_version: 1,
        attempt: attempt.clone(),
        observation: observation.clone(),
        socket: root.join("observation-proxy.sock"),
        identity_file: root.join("workspace/identity.json"),
        private_repository: metadata.clone(),
        private_git: private_git.clone(),
        projection_state: root.join("projection"),
        cas_root: store.root().canonicalize()?,
        policy,
        budget: ProjectionBudget::default(),
    };
    let service = CommandServiceConfig {
        format_version: 1,
        view: view.clone(),
        binary: binary.into(),
        state_directory: root.join("command-state"),
        git_config_sha256: hex::encode(Sha256::digest(fs::read(private_git.join("config"))?)),
        user_name: "Rudder Managed Agent".into(),
        user_email: "managed-agent@invalid.example".into(),
        command_timeout_ms: 90_000,
    };
    let config = services::Config {
        view,
        service,
        authority: authority.client.clone(),
        workspace_socket: root.join("workspace/workspace.sock"),
        private_command_endpoint: root.join("command-endpoint"),
        public_command_endpoint: public_command_endpoint.clone(),
        control: root.join("control"),
    };
    wire::write_json(&root.join("config.json"), &config)?;
    // Fixture identity satisfies the sealed projection shape only. Actual
    // authorization is the separately scoped outer-controller service above.
    wire::write_json(
        &root.join("broker/identity.json"),
        &rudder_engine::agent_api::AgentIdentityFile {
            format_version: 1,
            project_id: attempt.project_id.clone(),
            run_id: attempt.run_id.clone(),
            run_epoch: attempt.run_epoch,
            owner_epoch: owner.epoch,
            token: format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple()),
        },
    )?;
    let spec = ExecutionSpec {
        format_version: 2,
        execution_id: observation.attempt.execution_id.clone(),
        operation_id: observation.attempt.operation_id.clone(),
        execution_root: root.join("executions"),
        manager_root: manager.root().into(),
        store_root: store.root().into(),
        authority_roots: vec![
            root.join("authority"),
            metadata.clone(),
            root.join("projection"),
            root.join("command-state"),
            root.join("command-endpoint"),
            root.join("config.json"),
            root.join("control"),
        ],
        view_id,
        owner,
        command: vec![
            "/opt/runtime/qualify-vcs".into(),
            "guest".into(),
            mode.into(),
        ],
        environment: BTreeMap::from([(
            "FORBIDDEN_PATHS".into(),
            serde_json::to_string(&[
                root.join("config.json"),
                metadata.join(".jj/working_copy/rudder-state.json"),
                store.object_path(&attempt.seed)?,
                manager
                    .root()
                    .join("views")
                    .join(&attempt.view_id)
                    .join("namespace.sqlite"),
            ])?,
        )]),
        runtime: vec![
            ReadOnlyRuntime {
                source: "/usr".into(),
                guest_path: "/usr".into(),
            },
            ReadOnlyRuntime {
                source: std::env::current_exe()?,
                guest_path: "/opt/runtime/qualify-vcs".into(),
            },
            ReadOnlyRuntime {
                source: binary.into(),
                guest_path: "/opt/runtime/rudder-jj".into(),
            },
            ReadOnlyRuntime {
                source: public_command_endpoint,
                guest_path: "/opt/runtime/jj-command".into(),
            },
        ],
        home_files: vec![],
        broker_directory: Some(root.join("broker")),
        broker_binding: Some(BrokerBinding {
            project_id: attempt.project_id,
            run_id: attempt.run_id,
            run_epoch: attempt.run_epoch,
            owner_epoch: attempt.owner.epoch,
        }),
        workspace_observation: Some(WorkspaceObservationProfile {
            binding: observation,
            endpoint_directory: root.join("workspace"),
            application: None,
        }),
        timeout_seconds: 180,
    };
    spec.validate()?;
    Ok(Fixture {
        spec,
        config,
        authority: Some(authority),
        store,
    })
}
fn control(f: &Fixture, action: &str) -> Result<serde_json::Value> {
    let id = Uuid::new_v4().to_string();
    wire::write_json(
        &f.config.control.join("request.json"),
        &serde_json::json!({"id":id,"action":action}),
    )?;
    let end = Instant::now() + Duration::from_secs(20);
    loop {
        ensure!(Instant::now() < end, "fixture control {action} deadline");
        let error = f.config.control.join("bridge-error.json");
        if error.exists() {
            bail!(
                "bridge startup/control failed: {}",
                fs::read_to_string(error)?
            );
        }
        let path = f.config.control.join("result.json");
        if path.exists() {
            let value: serde_json::Value = wire::read_json(&path)?;
            if value["id"] == id {
                return Ok(value);
            }
        }
        thread::sleep(Duration::from_millis(10));
    }
}
fn descendants(parent: u32) -> Result<Vec<u32>> {
    let mut known = BTreeSet::from([parent]);
    loop {
        let mut changed = false;
        for entry in fs::read_dir("/proc")?.filter_map(Result::ok) {
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
                continue;
            };
            let Ok(status) = fs::read_to_string(format!("/proc/{pid}/status")) else {
                continue;
            };
            let ppid = status.lines().find_map(|line| {
                line.strip_prefix("PPid:")
                    .and_then(|v| v.trim().parse::<u32>().ok())
            });
            if ppid.is_some_and(|p| known.contains(&p)) {
                changed |= known.insert(pid);
            }
        }
        if !changed {
            break;
        }
    }
    Ok(known.into_iter().collect())
}
fn witnesses(parent: u32) -> Result<Vec<(u32, OwnedFd)>> {
    descendants(parent)?
        .into_iter()
        .map(|pid| {
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
            ensure!(fd >= 0, "cannot pin witnessed process {pid}");
            Ok((pid, unsafe { OwnedFd::from_raw_fd(fd as i32) }))
        })
        .collect()
}
fn exited(fd: &OwnedFd) -> Result<bool> {
    let mut poll = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let n = unsafe { libc::poll(&mut poll, 1, 0) };
    ensure!(n >= 0, "pidfd poll failed");
    Ok(n > 0)
}
struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            unsafe {
                libc::kill(-(self.0.id() as i32), libc::SIGKILL);
            }
            let _ = self.0.wait();
        }
    }
}
fn run(f: Fixture, mode: &str) -> Result<serde_json::Value> {
    let handoff = ExecutionHandoff::new(&f.spec)?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .args(handoff.helper_args())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        });
    }
    let mut child = ChildGuard(command.spawn()?);
    let receipt = handoff.activate(child.0.id())?;
    let mut input = child.0.stdin.take().context("helper stdin absent")?;
    let output = child.0.stdout.take().context("helper stdout absent")?;
    let (send, receive) = mpsc::sync_channel(256);
    let reader = thread::spawn(move || {
        for line in BufReader::new(output).lines() {
            let Ok(line) = line else {
                break;
            };
            if send.send(line).is_err() {
                break;
            }
        }
    });
    let mut events = Vec::new();
    let mut seen = BTreeSet::new();
    let mut pidfds = Vec::new();
    let mut writer_lock: Option<File> = None;
    let mut guest_proof = None;
    let end = Instant::now() + Duration::from_secs(220);
    let status = loop {
        ensure!(Instant::now() < end, "mounted helper deadline");
        if f.config.control.join("bridge-error.json").exists() {
            bail!(
                "bridge failed: {}",
                fs::read_to_string(f.config.control.join("bridge-error.json"))?
            );
        }
        for line in receive.try_iter() {
            println!("{line}");
            if line.starts_with('{') {
                let value: serde_json::Value = serde_json::from_str(&line)?;
                if value["kind"] == "mounted-vcs-guest" {
                    guest_proof = Some(value);
                }
            }
            let Some(marker) = line.strip_prefix("VCS-FIXTURE:") else {
                continue;
            };
            ensure!(
                seen.insert(marker.to_string()),
                "duplicate fixture transition"
            );
            for path in serde_json::from_str::<Vec<String>>(&f.spec.environment["FORBIDDEN_PATHS"])?
            {
                ensure!(
                    Path::new(&path).is_file(),
                    "host-side forbidden-file negative control is missing"
                );
            }
            let action = match marker {
                "CAPTURE_READY" => Some("capture-lost-reply"),
                "CAPTURE_LATER" => Some("retire-original-capture"),
                "ARM_LOST_COMMAND" => Some("drop-next-command"),
                "FINALIZE" | "EPOCH_READY" => Some("stop"),
                marker if marker.starts_with("EDIT_SELECTED:") => None,
                marker if marker.starts_with("COMMAND_REPLY_LOST:") => {
                    Some("restart-command-service")
                }
                _ => bail!("unknown guest transition"),
            };
            let lost_id = marker.strip_prefix("COMMAND_REPLY_LOST:");
            let before = if let Some(id) = lost_id {
                Uuid::parse_str(id)?;
                let bytes = fs::read(
                    f.config
                        .service
                        .state_directory
                        .join("receipts")
                        .join(format!("{id}.json")),
                )?;
                let receipt: CommandReceipt = serde_json::from_slice(&bytes)?;
                ensure!(
                    receipt.state == CommandState::Completed && receipt.exit_code == Some(0),
                    "dropped response did not follow durable completion"
                );
                Some(bytes)
            } else {
                None
            };
            if let Some(action) = action {
                events.push(control(&f, action)?);
            }
            if let Some(expected) = marker.strip_prefix("EDIT_SELECTED:") {
                let id = rudder_vcs_bridge::GitCommitId(expected.into());
                id.validate()?;
                let file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(f.config.view.private_repository.join("rudder-command.lock"))?;
                file.try_lock_exclusive()
                    .context("same-tree Edit writer has not settled")?;
                let repo = services::current_repo(&f.config.view)?;
                let current = repo
                    .view()
                    .get_wc_commit_id(WorkspaceName::DEFAULT)
                    .context("edited WC absent")?;
                ensure!(
                    current.hex() == expected,
                    "same-tree edit selected another commit"
                );
                let baseline =
                    repo.store()
                        .get_commit(&jj_lib::backend::CommitId::new(hex::decode(
                            &f.config.view.attempt.baseline.commit.0,
                        )?))?;
                ensure!(
                    CommitIdentity::from_jj(&baseline) == f.config.view.attempt.baseline,
                    "canonical baseline identity changed"
                );
                events.push(serde_json::json!({"action":"verify-same-tree-edit","exact_private_wc_commit":expected,"canonical_baseline_preserved":true,"under_private_writer_lock":true}));
            }
            if let Some(id) = lost_id {
                ensure!(
                    fs::read(
                        f.config
                            .service
                            .state_directory
                            .join("receipts")
                            .join(format!("{id}.json"))
                    )? == before.unwrap(),
                    "restart replaced original command receipt"
                );
            }
            if marker == "FINALIZE" || marker == "EPOCH_READY" {
                let file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .mode(0o600)
                    .open(f.config.view.private_repository.join("rudder-command.lock"))?;
                file.try_lock_exclusive()
                    .context("a command writer survived close/handler settlement")?;
                writer_lock = Some(file); // held until AFTER helper finalization
                pidfds = witnesses(child.0.id())?;
                ensure!(
                    pidfds.len() >= 4,
                    "source writer was not actually witnessed"
                );
                if marker == "EPOCH_READY" {
                    f.authority
                        .as_ref()
                        .unwrap()
                        .active
                        .store(false, Ordering::Release);
                    let stale = rudder_engine::vfs::rpc::ObservationClient::new(
                        &f.config.workspace_socket,
                        &f.config.view.identity_file,
                        f.config.view.observation.clone(),
                    )?;
                    ensure!(
                        stale
                            .call(rudder_engine::vfs::rpc::Operation::Cursor)
                            .is_err(),
                        "old workspace binding survived outer authority loss"
                    );
                    events.push(serde_json::json!({"action":"invalidate-outer-fixture-attempt","old_binding_denied":true,"journal_epoch_transfer_qualified":false}));
                    continue;
                }
            }
            input.write_all(b"continue\n")?;
            input.flush()?;
        }
        if let Some(status) = child.0.try_wait()? {
            break status;
        }
        thread::sleep(Duration::from_millis(10));
    };
    drop(receive);
    reader
        .join()
        .map_err(|_| anyhow::anyhow!("stdout reader failed"))?;
    receipt.wait(Duration::from_secs(2))?;
    ensure!(
        writer_lock.is_some(),
        "host did not close ingress/hold writer lock before finalization"
    );
    let end = Instant::now() + Duration::from_secs(30);
    loop {
        let alive: Vec<_> = pidfds
            .iter()
            .filter_map(|(pid, fd)| match exited(fd) {
                Ok(true) => None,
                _ => Some(*pid),
            })
            .collect();
        if alive.is_empty() {
            break;
        }
        ensure!(
            Instant::now() < end,
            "witnessed writers survived kernel drain: {alive:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }
    let record: serde_json::Value = wire::read_json(
        &f.spec
            .execution_root
            .join("runs")
            .join(&f.spec.execution_id)
            .join("record.json"),
    )?;
    let record = &record["record"];
    let mut proof = serde_json::json!({"mode":mode,"execution_id":f.spec.execution_id,"view_id":f.spec.view_id,"phase":record["phase"],"events":events,"witnessed_processes":pidfds.len(),"all_pidfds_exited":true,"command_ingress_closed_and_handlers_settled":true,"canonical_command_writer_lock_held_through_helper_finalization":true,"authority":"fixture outer-controller service; not native AgentBroker"});
    if mode == "live" {
        ensure!(
            status.success() && record["phase"] == "checkpointed" && record["agent_exit"] == 0,
            "normal helper did not acknowledge checkpoint"
        );
        let checkpoint: rudder_engine::vfs::VfsCheckpoint =
            serde_json::from_value(record["checkpoint"].clone())?;
        ensure!(
            services::source_bytes(&f.store, &checkpoint.directory, b"source.ts")?
                == b"export const n = 5;\n",
            "terminal checkpoint omitted post-observation FD write"
        );
        ensure!(
            !services::source_bytes(&f.store, &checkpoint.directory, b"writer.log")?.is_empty(),
            "writer was not active in mounted view"
        );
        let state: serde_json::Value =
            wire::read_json(&f.config.view.working_copy_state().join("rudder-state.json"))?;
        let current: rudder_vcs_bridge::LiveObservation =
            serde_json::from_value(state["observation"].clone())?;
        ensure!(
            services::source_bytes(&f.store, &current.directory, b"source.ts")?
                == b"export const n = 4;\n",
            "latest jj observation unexpectedly included later FD write"
        );
        ensure!(
            !f.config.view.private_repository.join("source.ts").exists(),
            "private metadata acquired a scanner/materialized source fallback"
        );
        proof["guest"] = guest_proof.context("actual guest proof absent")?;
        proof["checkpoint"] = serde_json::to_value(checkpoint)?;
        proof["latest_jj_source_number"] = 4.into();
        proof["terminal_checkpoint_source_number"] = 5.into();
        proof["source_files_in_private_metadata"] = 0.into();
    } else {
        ensure!(
            !status.success() && record["phase"] == "orphaned" && record["checkpoint"].is_null(),
            "revoked helper claimed a checkpoint"
        );
        proof["no_checkpoint_on_authority_loss"] = true.into();
    }
    drop(writer_lock);
    Ok(proof)
}
fn qualify(binary: &Path, root: &Path) -> Result<()> {
    ensure!(
        unsafe { libc::geteuid() } == 1500 && unsafe { libc::getegid() } == 1500,
        "outer qualification must use UID/GID1500"
    );
    let namespace = fs::read_link("/proc/self/ns/mnt")?;
    wire::private(root)?;
    let mut records = Vec::new();
    for mode in ["live", "epoch"] {
        let f = pollster::block_on(fixture(&root.join(mode), binary, mode))?;
        records.push(run(f, mode)?);
    }
    ensure!(
        fs::read_link("/proc/self/ns/mnt")? == namespace,
        "outer controller mount namespace changed"
    );
    println!(
        "{}",
        serde_json::json!({"format_version":1,"kind":"mounted-jj-rpc-consumer","uid":unsafe{libc::geteuid()},"gid":unsafe{libc::getegid()},"parent_mount_namespace_unchanged":true,"production_default_changed":false,"native_broker_qualified":false,"observation_consistency":"live_best_effort","records":records})
    );
    Ok(())
}
