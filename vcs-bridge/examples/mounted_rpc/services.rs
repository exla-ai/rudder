use super::{authority, wire};
use anyhow::{Context, Result, bail, ensure};
use jj_lib::gitignore::GitIgnoreFile;
use jj_lib::matchers::{EverythingMatcher, NothingMatcher};
use jj_lib::object_id::ObjectId;
use jj_lib::repo::{Repo, RepoLoader, StoreFactories};
use jj_lib::working_copy::SnapshotOptions;
use rudder_engine::execution::{ExecutionSpec, HelperServices, HelperServicesFactory};
use rudder_engine::store::LocalStore;
use rudder_engine::vfs::rpc::{ObservationClient, Operation, Reply};
use rudder_vcs_bridge::command_transport::*;
use rudder_vcs_bridge::rpc_view::*;
use rudder_vcs_bridge::working_copy::*;
use rudder_vcs_bridge::{JjTreeIdentity, LiveObservation};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Arc, atomic::Ordering};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Clone, Serialize, Deserialize)]
pub struct Config {
    pub view: RpcViewConfig,
    pub service: CommandServiceConfig,
    pub authority: authority::Client,
    pub workspace_socket: PathBuf,
    pub private_command_endpoint: PathBuf,
    pub public_command_endpoint: PathBuf,
    pub control: PathBuf,
}
pub fn settings() -> jj_lib::settings::UserSettings {
    use jj_lib::config::{ConfigLayer, ConfigSource, StackedConfig};
    let mut stack = StackedConfig::with_defaults();
    let mut layer = ConfigLayer::empty(ConfigSource::User);
    layer
        .set_value("user.name", "Rudder Managed Agent")
        .unwrap();
    layer
        .set_value("user.email", "managed-agent@invalid.example")
        .unwrap();
    stack.add_layer(layer);
    jj_lib::settings::UserSettings::from_config(stack).unwrap()
}
pub fn factory() -> HelperServicesFactory {
    Box::new(|spec: &ExecutionSpec| {
        // Execution invokes this only after fresh single-thread namespace entry.
        let config: Config = wire::read_json(
            &spec
                .execution_root
                .parent()
                .context("fixture parent absent")?
                .join("config.json"),
        )?;
        ensure!(
            spec.workspace_observation
                .as_ref()
                .context("workspace absent")?
                .binding
                == config.view.observation,
            "sealed execution/config mismatch"
        );
        let authority = Arc::new(config.authority.clone());
        let witness = Arc::new(DurableWorkingCopyWitness::new(
            config.view.clone(),
            settings(),
        )?);
        let errors = config.control.join("bridge-error.json");
        thread::Builder::new()
            .name("fixture-vcs-controller".into())
            .spawn(move || {
                if let Err(error) = run(&config) {
                    let _ = wire::write_json(
                        &errors,
                        &serde_json::json!({"error":format!("{error:#}")}),
                    );
                }
            })?;
        Ok(HelperServices {
            workspace_authority: authority,
            adoption_witness: Some(witness),
            application: None,
            workspace_factory: None,
        })
    })
}
fn provider(config: &Config) -> Result<Arc<RpcWorkspaceView>> {
    Ok(Arc::new(RpcWorkspaceView::new(
        config.view.clone(),
        LocalStore::open(&config.view.cas_root)?,
        Arc::new(config.authority.clone()),
    )?))
}
pub fn source_bytes(
    store: &LocalStore,
    root: &rudder_engine::protocol::ObjectRef,
    name: &[u8],
) -> Result<Vec<u8>> {
    let entry = rudder_engine::tree::lookup(store, root, name)?.context("source file absent")?;
    let rudder_engine::directory::EntryNode::File {
        object,
        logical_size,
        ..
    } = entry.node
    else {
        bail!("source is not a file");
    };
    ensure!(logical_size <= 64 * 1024, "fixture file too large");
    rudder_engine::content::read_file_range(store, &object, 0, logical_size as usize)
}
pub fn current_repo(config: &RpcViewConfig) -> Result<Arc<jj_lib::repo::ReadonlyRepo>> {
    let loader = RepoLoader::init_from_file_system(
        &settings(),
        &config.private_repository.join(".jj/repo"),
        &StoreFactories::default(),
    )?;
    pollster::block_on(async {
        let heads = loader.op_heads_store().get_op_heads().await?;
        ensure!(heads.len() == 1, "private operation heads diverged");
        let operation = loader.load_operation(&heads[0]).await?;
        Ok(loader.load_at(&operation).await?)
    })
}
fn publish_client(config: &Config, service: &CommandService) -> Result<()> {
    let host = service.client_config();
    let identity: serde_json::Value = wire::read_json(&host.identity_file)?;
    wire::write_json(
        &config.public_command_endpoint.join("identity.json"),
        &identity,
    )?;
    wire::write_json(
        &config.public_command_endpoint.join("client.json"),
        &CommandClientConfig {
            format_version: 1,
            socket: "/opt/runtime/jj-command/jj.sock".into(),
            identity_file: "/opt/runtime/jj-command/identity.json".into(),
            binding: host.binding,
        },
    )
}
fn run(config: &Config) -> Result<()> {
    wire::wait_file(&config.workspace_socket, 20)?;
    let observation_proxy =
        wire::ReplyProxy::bind(config.view.socket.clone(), config.workspace_socket.clone())?;
    let view = provider(config)?;
    let repo = current_repo(&config.view)?;
    let baseline = config
        .view
        .attempt
        .baseline
        .tree
        .to_jj(repo.store().clone())?;
    let observed = view.prepare_baseline(&baseline, None)?;
    pollster::block_on(bind_prepared_working_copy(
        &config.view.private_repository,
        &config.view.attempt,
        &settings(),
        view.clone(),
        observed,
    ))?;
    let attempt = pollster::block_on(rudder_vcs_bridge::attempt::initialize_attempt_history(
        &config.view.private_repository,
        &config.view.attempt,
        &settings(),
        view.clone(),
    ))?;
    ensure!(
        attempt.working_copy.change != config.view.attempt.baseline.change,
        "attempt inherited canonical change id"
    );
    let mut service = Some(CommandService::bind(
        config.service.clone(),
        &config.private_command_endpoint,
    )?);
    let command_proxy = wire::ReplyProxy::bind(
        config.public_command_endpoint.join("jj.sock"),
        config.private_command_endpoint.join("jj.sock"),
    )?;
    publish_client(config, service.as_ref().unwrap())?;
    wire::write_json(
        &config.control.join("ready.json"),
        &serde_json::json!({"ready":true,"attempt_history":attempt}),
    )?;
    let deadline = Instant::now() + Duration::from_secs(240);
    let mut last = String::new();
    let mut captured: Option<rudder_engine::vfs::rpc::ObservationReceipt> = None;
    while Instant::now() < deadline {
        let path = config.control.join("request.json");
        if path.exists() {
            let request: serde_json::Value = wire::read_json(&path)?;
            let id = request["id"].as_str().context("control id absent")?;
            if id != last {
                uuid::Uuid::parse_str(id)?;
                let action = request["action"]
                    .as_str()
                    .context("control action absent")?;
                let result = match action {
                    "capture-lost-reply" => {
                        let repo = current_repo(&config.view)?;
                        let state: serde_json::Value = wire::read_json(
                            &config.view.working_copy_state().join("rudder-state.json"),
                        )?;
                        let previous: LiveObservation =
                            serde_json::from_value(state["observation"].clone())?;
                        let tree: JjTreeIdentity = serde_json::from_value(state["tree"].clone())?;
                        observation_proxy
                            .drop_observe
                            .store(true, Ordering::Release);
                        let options = SnapshotOptions {
                            base_ignores: GitIgnoreFile::empty(),
                            progress: None,
                            start_tracking_matcher: &EverythingMatcher,
                            force_tracking_matcher: &NothingMatcher,
                            max_new_file_size: u64::MAX,
                        };
                        ensure!(
                            pollster::block_on(view.observe(
                                &config.view.attempt,
                                Some(&previous),
                                &tree,
                                &options,
                                repo.store().clone()
                            ))
                            .is_err(),
                            "lost capture unexpectedly confirmed"
                        );
                        let count = observation_proxy.observes.load(Ordering::Acquire);
                        // Restart the consumer object while the helper still owns
                        // the SAME mounted view and durable observation receipt.
                        let restarted = provider(config)?;
                        let pending = restarted
                            .inspect_pending_capture()?
                            .context("original pending capture lost")?;
                        ensure!(pending.retained, "original capture is not retained");
                        let observed = pending
                            .observation
                            .as_ref()
                            .context("captured root absent")?;
                        let bytes = source_bytes(
                            &LocalStore::open(&config.view.cas_root)?,
                            &observed.directory,
                            b"source.ts",
                        )?;
                        ensure!(bytes == b"export const n = 2;\n", "captured bytes changed");
                        ensure!(
                            observation_proxy.observes.load(Ordering::Acquire) == count,
                            "consumer restart silently recaptured"
                        );
                        let result = serde_json::json!({"original_nonce":pending.nonce,"directory":observed.directory,"bytes_hex":hex::encode(bytes),"retained":true,"consumer_restarted":true,"new_observe_calls":0});
                        captured = Some(pending);
                        result
                    }
                    "retire-original-capture" => {
                        let expected = captured.as_ref().context("capture proof absent")?;
                        let restarted = provider(config)?;
                        let actual = restarted
                            .inspect_pending_capture()?
                            .context("pending nonce absent after later write")?;
                        ensure!(
                            actual.nonce == expected.nonce
                                && actual.observation == expected.observation,
                            "pending capture was regenerated"
                        );
                        let observed = actual.observation.as_ref().unwrap();
                        let bytes = source_bytes(
                            &LocalStore::open(&config.view.cas_root)?,
                            &observed.directory,
                            b"source.ts",
                        )?;
                        ensure!(
                            bytes == b"export const n = 2;\n",
                            "retained original followed later FD writes"
                        );
                        let client = ObservationClient::new(
                            &config.view.socket,
                            &config.view.identity_file,
                            config.view.observation.clone(),
                        )?;
                        let Reply::Cursor(cursor) = client.call(Operation::Cursor)? else {
                            bail!("cursor absent");
                        };
                        ensure!(
                            cursor.generation > observed.end.generation,
                            "later FD write was not dirty"
                        );
                        restarted.retire_uncommitted_capture()?;
                        ensure!(
                            restarted.inspect_pending_capture()?.is_none(),
                            "pending capture not safely retired"
                        );
                        serde_json::json!({"same_original_nonce":actual.nonce,"same_directory":observed.directory,"later_generation":cursor.generation,"captured_generation":observed.end.generation,"retired_only_uncommitted_capture":true})
                    }
                    "drop-next-command" => {
                        command_proxy.drop_execute.store(true, Ordering::Release);
                        serde_json::json!({"armed":true})
                    }
                    "restart-command-service" => {
                        let before = current_repo(&config.view)?.op_id().clone();
                        let old = service.take().context("command service absent")?;
                        old.await_closed(Duration::from_secs(10))?;
                        drop(old);
                        service = Some(CommandService::bind(
                            config.service.clone(),
                            &config.private_command_endpoint,
                        )?);
                        publish_client(config, service.as_ref().unwrap())?;
                        ensure!(
                            current_repo(&config.view)?.op_id() == &before,
                            "service restart replayed private history"
                        );
                        serde_json::json!({"same_private_operation":before.hex(),"service_restarted":true,"no_history_replay":true})
                    }
                    "stop" => {
                        if let Some(old) = service.take() {
                            old.await_closed(Duration::from_secs(10))?;
                            drop(old);
                        }
                        drop(command_proxy);
                        drop(observation_proxy);
                        wire::write_json(
                            &config.control.join("result.json"),
                            &serde_json::json!({"id":id,"action":action,"result":{"command_ingress_closed":true,"handlers_settled":true}}),
                        )?;
                        return Ok(());
                    }
                    _ => bail!("unknown fixture lifecycle request"),
                };
                wire::write_json(
                    &config.control.join("result.json"),
                    &serde_json::json!({"id":id,"action":action,"result":result}),
                )?;
                last = id.into();
            }
        }
        thread::sleep(Duration::from_millis(5));
    }
    bail!("fixture bridge lifecycle deadline")
}
