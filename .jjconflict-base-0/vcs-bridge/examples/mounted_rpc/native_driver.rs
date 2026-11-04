//! Actual native AgentBroker + execution-v3 factory qualification.
use super::*;
use rudder_engine::execution::{HelperApplicationSpec, ProtectedHostContextRef};
use rudder_engine::journal::{CapabilityGrant, Journal, OwnerFence};
use rudder_engine::vfs::rpc::{ObservationClient, Operation};

const KIND: &str = "workspace-vcs-v1";
pub fn main(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("native-as-user") => {
            ensure!(
                args.len() == 4 && unsafe { libc::geteuid() } == 0,
                "requires owned VM root bootstrap"
            );
            unsafe {
                ensure!(
                    libc::setgroups(0, std::ptr::null()) == 0
                        && libc::setgid(1500) == 0
                        && libc::setuid(1500) == 0,
                    "cannot enter UID/GID1500"
                );
            }
            bail!(
                "fresh UID exec failed: {}",
                Command::new(std::env::current_exe()?)
                    .args(["native-qualify", &args[1], &args[2], &args[3]])
                    .exec()
            );
        }
        Some("native-qualify") => {
            ensure!(args.len() == 4, "native-qualify NATIVE CLI ROOT");
            qualify(
                Path::new(&args[1]),
                Path::new(&args[2]),
                Path::new(&args[3]),
            )
        }
        _ => bail!("unsupported native qualification mode"),
    }
}

struct NativeFixture {
    f: Fixture,
    journal: Journal,
    owner: OwnerFence,
    _broker: ChildGuard,
}
fn prepare(native: &Path, binary: &Path, root: &Path, mode: &str) -> Result<NativeFixture> {
    let mut f = pollster::block_on(fixture(root, binary, &format!("native-{mode}")))?;
    // No fixture authority service remains. Native connects only to the real
    // external AgentBroker and its journal-backed exact workspace capability.
    drop(f.authority.take());
    fs::remove_file(root.join("config.json"))?;
    f.config.view.attempt.attempt_id = f.spec.execution_id.clone();
    f.config.view.socket = f.config.workspace_socket.clone();
    f.config.service.view = f.config.view.clone();
    f.config.service.command_timeout_ms = 20_000;
    let mut journal = Journal::open(root.join("authority/journal.sqlite"))?;
    let binding = &f.config.view.attempt;
    let owner = journal.create_project(
        &binding.project_id,
        &binding.owner.owner,
        &binding.baseline.commit.0,
        0,
    )?;
    let epoch = journal.observe_run_attempt(&owner, &binding.run_id, &f.spec.execution_id, 0)?;
    ensure!(
        epoch == binding.run_epoch,
        "unexpected journal attempt epoch"
    );
    let grant = CapabilityGrant {
        principal: format!("agent:{}", binding.run_id),
        run_id: binding.run_id.clone(),
        run_epoch: epoch,
        operations: BTreeSet::from(["workspace:observe".into()]),
        recipients: BTreeMap::new(),
        scopes: BTreeSet::from([rudder_engine::execution::workspace_scope(
            &f.config.view.observation,
        )?]),
        topics: BTreeSet::new(),
        expires_at_ms: None,
    };
    let identity = rudder_engine::agent_api::AgentIdentityFile {
        format_version: 1,
        project_id: binding.project_id.clone(),
        run_id: binding.run_id.clone(),
        run_epoch: epoch,
        owner_epoch: binding.owner.epoch,
        token: journal.issue_capability(&owner, &grant, 0)?,
    };
    wire::write_json(&root.join("broker/identity.json"), &identity)?;
    let mut broker_command = Command::new(native);
    broker_command
        .args([
            "fabric",
            "broker",
            "--journal",
            root.join("authority/journal.sqlite").to_str().unwrap(),
            "--identity-file",
            root.join("broker/identity.json").to_str().unwrap(),
            "--socket",
            root.join("broker/broker.sock").to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    unsafe {
        broker_command.pre_exec(|| {
            if libc::setpgid(0, 0) == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        });
    }
    let broker = ChildGuard(broker_command.spawn()?);
    wire::wait_file(&root.join("broker/broker.sock"), 10)?;
    let executable = fs::read(binary)?;
    let context = serde_json::json!({"format_version":1,"kind":KIND,"service":f.config.service,"executable":{"sha256":hex::encode(Sha256::digest(&executable)),"size_bytes":executable.len()},"guest_executable":"/opt/runtime/rudder-jj"});
    let bytes = canonical_json(&context)?;
    let context_path = root.join("authority/vcs-context.json");
    wire::write_json(&context_path, &context)?;
    ensure!(
        fs::read(&context_path)? == bytes,
        "fixture wrote noncanonical protected context"
    );
    f.spec.format_version = 3;
    f.spec.workspace_observation.as_mut().unwrap().application = Some(HelperApplicationSpec {
        format_version: 1,
        kind: KIND.into(),
        host_context: ProtectedHostContextRef {
            format_version: 1,
            path: context_path.clone(),
            sha256: hex::encode(Sha256::digest(&bytes)),
            size_bytes: bytes.len() as u64,
        },
        projection_directory: f.config.public_command_endpoint.clone(),
    });
    f.spec
        .runtime
        .retain(|runtime| runtime.guest_path != "/opt/runtime/jj-command");
    f.spec.environment.insert(
        "FORBIDDEN_PATHS".into(),
        serde_json::to_string(&[
            context_path,
            root.join("authority/journal.sqlite"),
            f.config
                .view
                .private_repository
                .join(".jj/working_copy/rudder-state.json"),
            f.store.object_path(&f.config.view.attempt.seed)?,
            f.spec
                .manager_root
                .join("views")
                .join(&f.spec.view_id)
                .join("namespace.sqlite"),
        ])?,
    );
    f.spec.validate()?;
    Ok(NativeFixture {
        f,
        journal,
        owner,
        _broker: broker,
    })
}
fn continue_guest(input: &mut impl Write) -> Result<()> {
    input.write_all(b"continue\n")?;
    input.flush()?;
    Ok(())
}
fn run(mut native_fixture: NativeFixture, native: &Path, mode: &str) -> Result<serde_json::Value> {
    let f = &native_fixture.f;
    let handoff = ExecutionHandoff::new(&f.spec)?;
    let mut command = Command::new(native);
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
    let mut input = child.0.stdin.take().context("native helper stdin absent")?;
    let output = child
        .0
        .stdout
        .take()
        .context("native helper stdout absent")?;
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
    let mut seen = BTreeSet::new();
    let mut pidfds = Vec::new();
    let mut guest = None;
    let mut events = Vec::new();
    let end = Instant::now() + Duration::from_secs(150);
    let status = loop {
        ensure!(Instant::now() < end, "actual native v3 helper deadline");
        for line in receive.try_iter() {
            println!("{line}");
            if line.starts_with('{') {
                let value: serde_json::Value = serde_json::from_str(&line)?;
                if value["kind"] == "mounted-native-vcs-guest" {
                    guest = Some(value);
                }
            }
            let Some(marker) = line.strip_prefix("VCS-FIXTURE:") else {
                continue;
            };
            ensure!(seen.insert(marker.to_owned()), "duplicate guest transition");
            for path in serde_json::from_str::<Vec<String>>(&f.spec.environment["FORBIDDEN_PATHS"])?
            {
                ensure!(
                    Path::new(&path).is_file(),
                    "missing host-side negative control"
                );
            }
            if let Some(expected) = marker.strip_prefix("EDIT_SELECTED:") {
                rudder_vcs_bridge::GitCommitId(expected.into()).validate()?;
                let lock = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(f.config.view.private_repository.join("rudder-command.lock"))?;
                lock.try_lock_exclusive()?;
                let repo = services::current_repo(&f.config.view)?;
                ensure!(
                    repo.view()
                        .get_wc_commit_id(WorkspaceName::DEFAULT)
                        .context("native WC absent")?
                        .hex()
                        == expected,
                    "native same-tree edit selected wrong commit"
                );
                let baseline =
                    repo.store()
                        .get_commit(&jj_lib::backend::CommitId::new(hex::decode(
                            &f.config.view.attempt.baseline.commit.0,
                        )?))?;
                ensure!(
                    CommitIdentity::from_jj(&baseline) == f.config.view.attempt.baseline,
                    "native factory changed canonical baseline"
                );
                events.push(serde_json::json!({"action":"verify-native-same-tree-edit","exact_private_wc_commit":expected,"baseline_preserved":true}));
                drop(lock);
                continue_guest(&mut input)?;
            } else if marker == "FINALIZE" {
                pidfds = witnesses(child.0.id())?;
                ensure!(pidfds.len() >= 5, "actual source writer not witnessed");
                // No fixture quiescence service or manual writer-lock handoff.
                // The actual native helper must perform its owned lifecycle.
                continue_guest(&mut input)?;
            } else if marker == "EPOCH_READY" {
                pidfds = witnesses(child.0.id())?;
                ensure!(pidfds.len() >= 5, "actual source writer not witnessed");
                let old = f.config.view.attempt.run_epoch;
                let new = native_fixture.journal.observe_run_attempt(
                    &native_fixture.owner,
                    &f.config.view.attempt.run_id,
                    &Uuid::new_v4().to_string(),
                    1,
                )?;
                ensure!(new > old, "real journal epoch did not advance");
                let client = ObservationClient::new(
                    &f.config.workspace_socket,
                    &f.config.view.identity_file,
                    f.config.view.observation.clone(),
                )?;
                ensure!(
                    client.call(Operation::Cursor).is_err(),
                    "real broker accepted retired workspace execution"
                );
                events.push(serde_json::json!({"action":"real-journal-attempt-replacement","old_run_epoch":old,"new_run_epoch":new,"old_workspace_rpc_denied":true}));
            } else {
                bail!("unexpected native guest transition {marker}");
            }
        }
        if let Some(status) = child.0.try_wait()? {
            break status;
        }
        thread::sleep(Duration::from_millis(10));
    };
    drop(receive);
    reader
        .join()
        .map_err(|_| anyhow::anyhow!("native output reader failed"))?;
    receipt.wait(Duration::from_secs(2))?;
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
            "native helper left exact witnessed writers alive: {alive:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }
    ensure!(
        !pidfds.is_empty(),
        "native helper never reached writer proof"
    );
    let envelope: serde_json::Value = wire::read_json(
        &f.spec
            .execution_root
            .join("runs")
            .join(&f.spec.execution_id)
            .join("record.json"),
    )?;
    let record = &envelope["record"];
    let mut proof = serde_json::json!({"mode":mode,"execution_id":f.spec.execution_id,"phase":record["phase"],"native_agent_broker":true,"fixture_authority_used":false,"execution_spec_version":3,"actual_native_factory":true,"witnessed_processes":pidfds.len(),"all_pidfds_exited":true,"events":events,"manual_fixture_command_shutdown":false});
    if mode == "live" {
        ensure!(
            status.success() && record["phase"] == "checkpointed" && record["agent_exit"] == 0,
            "native v3 helper did not acknowledge clean checkpoint"
        );
        let checkpoint = match serde_json::from_str::<rudder_engine::execution::ExecutionRecord>(&record.to_string())? {
            rudder_engine::execution::ExecutionRecord::V1(record) => record.checkpoint.context("legacy fixture requires complete local checkpoint")?,
            rudder_engine::execution::ExecutionRecord::V2(_) => anyhow::bail!("legacy fixture cannot consume Record2 or partial checkpoint"),
        };
        ensure!(
            services::source_bytes(&f.store, &checkpoint.directory, b"source.ts")?
                == b"export const n = 5;\n",
            "native checkpoint omitted later FD write"
        );
        ensure!(
            !services::source_bytes(&f.store, &checkpoint.directory, b"writer.log")?.is_empty(),
            "native checkpoint omitted active writer bytes"
        );
        let state: serde_json::Value =
            wire::read_json(&f.config.view.working_copy_state().join("rudder-state.json"))?;
        let current: rudder_vcs_bridge::LiveObservation =
            serde_json::from_value(state["observation"].clone())?;
        ensure!(
            services::source_bytes(&f.store, &current.directory, b"source.ts")?
                == b"export const n = 4;\n",
            "native jj observation silently followed later FD writes"
        );
        ensure!(
            !f.config.view.private_repository.join("source.ts").exists(),
            "native provider fell back to materialized/scanned source"
        );
        proof["guest"] = guest.context("actual native guest proof absent")?;
        proof["checkpoint"] = serde_json::to_value(checkpoint)?;
        proof["latest_jj_source_number"] = 4.into();
        proof["terminal_checkpoint_source_number"] = 5.into();
        proof["native_shutdown_guard_then_checkpoint"] = true.into();
    } else {
        ensure!(
            !status.success() && record["phase"] == "orphaned" && record["checkpoint"].is_null(),
            "retired native execution claimed a checkpoint"
        );
        proof["no_checkpoint_on_actual_journal_epoch_change"] = true.into();
    }
    Ok(proof)
}
fn qualify(native: &Path, binary: &Path, root: &Path) -> Result<()> {
    ensure!(
        unsafe { libc::geteuid() } == 1500 && unsafe { libc::getegid() } == 1500,
        "native fixture requires UID/GID1500"
    );
    let namespace = fs::read_link("/proc/self/ns/mnt")?;
    wire::private(root)?;
    let mut records = Vec::new();
    for mode in ["live", "epoch"] {
        records.push(run(
            prepare(native, binary, &root.join(mode), mode)?,
            native,
            mode,
        )?);
    }
    ensure!(
        fs::read_link("/proc/self/ns/mnt")? == namespace,
        "outer native controller changed mount namespace"
    );
    println!(
        "{}",
        serde_json::json!({"format_version":1,"kind":"mounted-native-jj-rpc-consumer","uid":unsafe{libc::geteuid()},"gid":unsafe{libc::getegid()},"parent_mount_namespace_unchanged":true,"production_default_changed":false,"native_broker_qualified":true,"explicit_protected_v3_factory":true,"records":records})
    );
    Ok(())
}
