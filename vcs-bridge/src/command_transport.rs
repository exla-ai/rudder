//! Scoped semantic jj transport. Wire data never becomes host argv, config,
//! cwd, editor, template or Git transport. The server builds fixed commands.

use crate::{atomic, rpc_view::*, vfs_provider, working_copy::*, *};
use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use jj_lib::config::{ConfigLayer, ConfigSource, StackedConfig};
use jj_lib::gitignore::GitIgnoreFile;
use jj_lib::matchers::{EverythingMatcher, NothingMatcher};
use jj_lib::object_id::ObjectId;
use jj_lib::ref_name::WorkspaceName;
use jj_lib::repo::{Repo, RepoLoader, StoreFactories};
use jj_lib::settings::UserSettings;
use jj_lib::working_copy::{SnapshotOptions, WorkingCopyFactory};
use rudder_engine::protocol::OwnerEpoch;
use rudder_engine::store::LocalStore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
            net::{UnixListener, UnixStream},
            process::CommandExt,
        },
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

const MAX_REQUEST: usize = 32 * 1024;
const MAX_RESPONSE: usize = 1024 * 1024;
const OUTPUT_LIMIT: usize = 128 * 1024;
const HOST_CONTEXT_ENV: &str = "RUDDER_JJ_HOST_CONTEXT";
const INHERITED_LOCK_ENV: &str = "RUDDER_JJ_WRITER_FD";
pub const ADAPTER_VERSION: &str = "0.40.0-rudder-bridge-v1";
const GUEST_HELP: &str = "Scoped workspace commands:\n  status\n  diff [--git]\n  log [--limit N] (1..100)\n  new -m MESSAGE\n  describe -m MESSAGE\n  edit FULL-COMMIT-ID (same source tree)\n  --version\n  --help\n";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub enum SemanticCommand {
    Status {},
    Diff {},
    Log { limit: u16 },
    New { message: String },
    Describe { message: String },
    Edit { commit: GitCommitId },
}
impl SemanticCommand {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Log { limit } => ensure!((1..=100).contains(limit), "log limit must be1..100"),
            Self::New { message } | Self::Describe { message } => ensure!(
                message.len() <= 8192 && !message.contains('\0'),
                "message exceeds supported literal bound"
            ),
            Self::Edit { commit } => commit.validate()?,
            _ => {}
        }
        Ok(())
    }
    fn args(&self) -> Vec<String> {
        match self {
            Self::Status {} => vec!["status".into()],
            Self::Diff {} => vec!["diff".into(), "--git".into()],
            Self::Log { limit } => vec![
                "log".into(), "--no-graph".into(), format!("--limit={limit}"),
                "--template=commit_id ++ \" \" ++ change_id.normal_hex() ++ \" \" ++ description.first_line() ++ \"\\n\"".into(),
            ],
            Self::New { message } => vec!["new".into(), format!("--message={message}")],
            Self::Describe { message } => vec!["describe".into(), format!("--message={message}")],
            Self::Edit { .. } => vec!["rudder-semantic-edit".into()],
        }
    }
}

/// Deliberately small CLI grammar. Unsupported flags are rejected before any
/// connection, observation, snapshot or private history publication.
pub fn parse_guest_arguments(args: &[String]) -> Result<SemanticCommand> {
    let command = match args {
        [name] if name == "status" || name == "st" => SemanticCommand::Status {},
        [name] if name == "diff" => SemanticCommand::Diff {},
        [name, flag] if name == "diff" && flag == "--git" => SemanticCommand::Diff {},
        [name] if name == "log" => SemanticCommand::Log { limit: 20 },
        [name, flag, value] if name == "log" && flag == "--limit" => SemanticCommand::Log {
            limit: value.parse()?,
        },
        [name, commit] if name == "edit" => SemanticCommand::Edit {
            commit: GitCommitId(commit.clone()),
        },
        [name, flag, value]
            if (name == "new" || name == "describe") && (flag == "-m" || flag == "--message") =>
        {
            if name == "new" {
                SemanticCommand::New {
                    message: value.clone(),
                }
            } else {
                SemanticCommand::Describe {
                    message: value.clone(),
                }
            }
        }
        [name, value]
            if (name == "new" || name == "describe") && value.starts_with("--message=") =>
        {
            if name == "new" {
                SemanticCommand::New {
                    message: value[10..].into(),
                }
            } else {
                SemanticCommand::Describe {
                    message: value[10..].into(),
                }
            }
        }
        _ => bail!(
            "unsupported managed jj command or flag; supported: status, diff, log, new -m, describe -m, edit <full commit id>"
        ),
    };
    command.validate()?;
    Ok(command)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandBinding {
    pub format_version: u32,
    pub project_id: String,
    pub owner: OwnerEpoch,
    pub run_id: String,
    pub run_epoch: u64,
    pub attempt_id: String,
    pub view_id: String,
}
impl From<&AttemptBinding> for CommandBinding {
    fn from(v: &AttemptBinding) -> Self {
        Self {
            format_version: 1,
            project_id: v.project_id.clone(),
            owner: v.owner.clone(),
            run_id: v.run_id.clone(),
            run_epoch: v.run_epoch,
            attempt_id: v.attempt_id.clone(),
            view_id: v.view_id.clone(),
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandIdentity {
    pub format_version: u32,
    pub binding: CommandBinding,
    pub token: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandClientConfig {
    pub format_version: u32,
    pub socket: PathBuf,
    pub identity_file: PathBuf,
    pub binding: CommandBinding,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandServiceConfig {
    pub format_version: u32,
    pub view: RpcViewConfig,
    pub binary: PathBuf,
    pub state_directory: PathBuf,
    pub git_config_sha256: String,
    pub user_name: String,
    pub user_email: String,
    pub command_timeout_ms: u64,
}
impl CommandServiceConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.format_version == 1,
            "unsupported semantic command service"
        );
        self.view.validate()?;
        ensure!(
            self.binary.is_absolute() && self.state_directory.is_absolute(),
            "host command paths must be absolute"
        );
        ensure!(
            self.git_config_sha256.len() == 64
                && self
                    .git_config_sha256
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit()),
            "invalid fixed Git config digest"
        );
        ensure!(
            self.user_name.len() <= 256
                && self.user_email.len() <= 256
                && !self.user_name.chars().any(char::is_control)
                && !self.user_email.chars().any(char::is_control),
            "invalid configured VCS author"
        );
        ensure!(
            (1000..=120_000).contains(&self.command_timeout_ms),
            "command timeout exceeds qualified bound"
        );
        Ok(())
    }
    fn check_private_configuration(&self) -> Result<()> {
        self.check_prepared_configuration()?;
        ensure!(
            fs::read_to_string(self.view.working_copy_state().join("type"))? == TYPE_NAME,
            "managed commands require an already bound Rudder WC, never a scanner fallback"
        );
        Ok(())
    }
    pub(crate) fn check_prepared_configuration(&self) -> Result<()> {
        self.validate()?;
        ensure!(
            matches!(
                fs::read_to_string(self.view.working_copy_state().join("type"))?.as_str(),
                "local" | TYPE_NAME
            ),
            "unsupported private working-copy type"
        );
        for path in [
            self.view.private_repository.join(".jj/config.toml"),
            self.view.private_repository.join(".jj/repo/config.toml"),
            self.view.private_git.join("config.worktree"),
        ] {
            ensure!(
                !path.try_exists()?,
                "private repo contains unqualified configuration"
            );
        }
        let config = read_private_or_git_file(&self.view.private_git.join("config"), 1024 * 1024)?;
        crate::seed::validate_private_git_config(&config)?;
        ensure!(
            hex::encode(Sha256::digest(config)) == self.git_config_sha256,
            "private Git config changed outside the command profile"
        );
        let hooks = self.view.private_git.join("hooks");
        ensure!(
            !hooks.try_exists()? || fs::read_dir(hooks)?.next().is_none(),
            "private repo contains unqualified hooks"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandState {
    Started,
    Completed,
    Uncertain,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandReceipt {
    pub format_version: u32,
    pub command_id: String,
    pub binding: CommandBinding,
    pub command: SemanticCommand,
    pub state: CommandState,
    pub before_heads: Vec<JjOperationId>,
    pub after_heads: Option<Vec<JjOperationId>>,
    pub backend: String,
    pub exit_code: Option<i32>,
    pub stdout_hex: String,
    pub stderr_hex: String,
    pub output_truncated: bool,
    pub note: Option<String>,
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
enum WireOperation {
    Execute {
        command_id: String,
        command: SemanticCommand,
    },
    Inspect {
        command_id: String,
    },
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    format_version: u32,
    identity: CommandIdentity,
    operation: WireOperation,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Response {
    format_version: u32,
    receipt: Option<CommandReceipt>,
    error: Option<String>,
}

struct FixedAuthority(AttemptBinding);
impl vfs_provider::AttemptAuthority for FixedAuthority {
    fn validate(&self, binding: &AttemptBinding) -> Result<()> {
        ensure!(
            *binding == self.0,
            "host VCS context belongs to another attempt"
        );
        Ok(())
    }
}
fn provider(config: &RpcViewConfig) -> Result<Arc<RpcWorkspaceView>> {
    Ok(Arc::new(RpcWorkspaceView::new(
        config.clone(),
        LocalStore::open(&config.cas_root)?,
        Arc::new(FixedAuthority(config.attempt.clone())),
    )?))
}
pub(crate) fn settings(config: &CommandServiceConfig) -> Result<UserSettings> {
    let mut stack = StackedConfig::with_defaults();
    stack.add_layer(fixed_layer(config)?);
    Ok(UserSettings::from_config(stack)?)
}
pub fn fixed_layer(config: &CommandServiceConfig) -> Result<ConfigLayer> {
    // CliRunner's public extension API requires a Default layer. The host
    // clears the environment, pins an empty user file and rejects repo config;
    // no guest config/alias/flag can override these fixed defaults.
    let mut layer = ConfigLayer::empty(ConfigSource::Default);
    layer.set_value("user.name", config.user_name.clone())?;
    layer.set_value("user.email", config.user_email.clone())?;
    layer.set_value("ui.paginate", "never")?;
    layer.set_value("ui.color", "never")?;
    layer.set_value("ui.editor", "false")?;
    layer.set_value("signing.behavior", "drop")?;
    Ok(layer)
}
fn heads(config: &CommandServiceConfig) -> Result<Vec<JjOperationId>> {
    let loader = RepoLoader::init_from_file_system(
        &settings(config)?,
        &config.view.private_repository.join(".jj/repo"),
        &StoreFactories::default(),
    )?;
    let mut out = pollster::block_on(loader.op_heads_store().get_op_heads())?
        .iter()
        .map(|id| JjOperationId(id.hex()))
        .collect::<Vec<_>>();
    out.sort();
    Ok(out)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostCommandContext {
    pub format_version: u32,
    pub service: CommandServiceConfig,
    pub command_id: String,
    pub command: SemanticCommand,
}
impl HostCommandContext {
    pub fn argv(&self) -> Vec<String> {
        let mut out = vec![format!("--rudder-command-id={}", self.command_id)];
        out.extend(self.command.args());
        out
    }
    pub fn view(&self) -> Result<Arc<RpcWorkspaceView>> {
        provider(&self.service.view)
    }
}

/// Only the trusted server supplies this protected context/environment. Exact
/// argv validation happens before CliRunner config, alias or workspace loading.
pub fn host_context_from_environment() -> Result<Option<HostCommandContext>> {
    let Some(path) = std::env::var_os(HOST_CONTEXT_ENV) else {
        return Ok(None);
    };
    let bytes = read_private_file(Path::new(&path), 128 * 1024)?;
    let context: HostCommandContext = serde_json::from_slice(&bytes)?;
    ensure!(
        context.format_version == 1,
        "unsupported host command context"
    );
    uuid::Uuid::parse_str(&context.command_id)?;
    context.command.validate()?;
    context.service.check_private_configuration()?;
    let receipt = read_receipt(&context.service, &context.command_id)?;
    ensure!(
        receipt.state == CommandState::Started && receipt.command == context.command,
        "host command is no longer the exact started request"
    );
    let active: ActiveCommand =
        atomic::read_json(&context.service.state_directory.join("active-command.json"))?;
    ensure!(
        active.command_id == context.command_id && active.binding == receipt.binding,
        "host command no longer owns its durable gate"
    );
    ensure!(
        heads(&context.service)? == receipt.before_heads,
        "private history changed before CLI startup; never replay"
    );
    ensure!(
        std::env::args().skip(1).collect::<Vec<_>>() == context.argv(),
        "managed CLI argv differs from its fixed semantic request"
    );
    ensure!(
        std::env::current_dir()?.canonicalize()?
            == context.service.view.private_repository.canonicalize()?,
        "managed CLI cwd changed"
    );
    let fd: i32 = std::env::var(INHERITED_LOCK_ENV)?.parse()?;
    ensure!(fd >= 3, "missing inherited private writer lock");
    let inherited = std::fs::File::from(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) });
    let expected = fs::metadata(
        context
            .service
            .view
            .private_repository
            .join("rudder-command.lock"),
    )?;
    let actual = inherited.metadata()?;
    ensure!(
        actual.is_file() && actual.dev() == expected.dev() && actual.ino() == expected.ino(),
        "inherited writer lock belongs to another private repo"
    );
    // Retain it for this process and synchronous Git descendants. A crashed
    // server cannot release ownership while its CLI child still writes.
    std::mem::forget(inherited);
    Ok(Some(context))
}

pub fn run_semantic_edit(context: &HostCommandContext) -> Result<()> {
    let SemanticCommand::Edit { commit } = &context.command else {
        bail!("expected semantic edit")
    };
    let view = context.view()?;
    let settings = settings(&context.service)?;
    pollster::block_on(async {
        let loader = RepoLoader::init_from_file_system(
            &settings,
            &context.service.view.private_repository.join(".jj/repo"),
            &StoreFactories::default(),
        )?;
        let heads = loader.op_heads_store().get_op_heads().await?;
        ensure!(
            heads.len() == 1,
            "private operation heads diverged; no automatic merge"
        );
        let operation = loader.load_operation(&heads[0]).await?;
        let repo = loader.load_at(&operation).await?;
        let target = repo
            .store()
            .get_commit(&jj_lib::backend::CommitId::new(hex::decode(&commit.0)?))?;
        ensure!(
            target.id() != repo.store().root_commit_id(),
            "root commit cannot be edited"
        );
        let wc = RudderWorkingCopyFactory::new(view.clone(), None).load_working_copy(
            repo.store().clone(),
            context.service.view.private_repository.clone(),
            context.service.view.working_copy_state(),
            &settings,
        )?;
        let mut locked = wc.start_mutation()?;
        let options = SnapshotOptions {
            base_ignores: GitIgnoreFile::empty(),
            progress: None,
            start_tracking_matcher: &EverythingMatcher,
            force_tracking_matcher: &NothingMatcher,
            max_new_file_size: u64::MAX,
        };
        let (observed, _) = locked.snapshot(&options).await?;
        ensure!(
            JjTreeIdentity::from_jj(&observed) == JjTreeIdentity::from_jj(&target.tree()),
            "edit would change source; mounted checkout is unsupported"
        );
        let mut tx = repo.start_transaction();
        tx.set_tag("rudder.command".into(), context.command_id.clone());
        tx.set_tag(
            "rudder.semantic".into(),
            "edit-same-observed-tree-v1".into(),
        );
        tx.repo_mut()
            .set_wc_commit(WorkspaceName::DEFAULT.to_owned(), target.id().clone())?;
        let changed = tx
            .commit(format!(
                "select existing commit {} with retained source",
                commit.0
            ))
            .await?;
        // One retained observation, no hidden second snapshot or checkout.
        locked.finish(changed.op_id().clone()).await?;
        println!(
            "Selected existing change {} at {}",
            target.change_id().hex(),
            target.id().hex()
        );
        Ok(())
    })
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ActiveCommand {
    format_version: u32,
    binding: CommandBinding,
    command_id: String,
}
struct Inner {
    config: CommandServiceConfig,
    identity: CommandIdentity,
    socket: PathBuf,
    stop: AtomicBool,
    active: AtomicUsize,
    receipts: AtomicUsize,
    accept_done: AtomicBool,
    start_gate: Mutex<()>,
    _owner: File,
}
pub struct CommandService {
    inner: Arc<Inner>,
}
/// Owned canonical writer fence. A trusted host holds this through source
/// sealing and its durable execution record; no peer boolean can create it.
pub struct CommandFinalizationGuard {
    _writer: File,
}
pub(crate) fn fence_private_writer(
    config: &CommandServiceConfig,
    timeout: Duration,
) -> Result<CommandFinalizationGuard> {
    let deadline = Instant::now() + timeout;
    let writer = lock_file(&config.view.private_repository.join("rudder-command.lock"))?;
    loop {
        match writer.try_lock_exclusive() {
            Ok(()) => return Ok(CommandFinalizationGuard { _writer: writer }),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                ensure!(
                    Instant::now() < deadline,
                    "a private VCS writer still owns its canonical fence"
                );
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error).context("acquire private VCS finalization fence"),
        }
    }
}
impl CommandService {
    pub fn bind(config: CommandServiceConfig, endpoint: &Path) -> Result<Self> {
        config.check_private_configuration()?;
        provider(&config.view)?.validate_authority(&config.view.attempt)?;
        vfs_provider::private_directory(&config.state_directory)?;
        vfs_provider::private_directory(&config.state_directory.join("receipts"))?;
        vfs_provider::private_directory(endpoint)?;
        let owner = lock_file(&config.state_directory.join("owner.lock"))?;
        owner
            .try_lock_exclusive()
            .context("private VCS command service is already owned")?;
        let writer = lock_file(&config.view.private_repository.join("rudder-command.lock"))?;
        writer
            .try_lock_exclusive()
            .context("a prior private VCS writer is still live")?;
        drop(writer);
        let mut receipt_count = 0;
        for entry in fs::read_dir(config.state_directory.join("receipts"))? {
            let path = entry?.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            receipt_count += 1;
            ensure!(
                receipt_count <= 4096,
                "private command receipt budget exhausted"
            );
            let mut receipt: CommandReceipt = atomic::read_json(&path)?;
            ensure!(
                receipt.binding == CommandBinding::from(&config.view.attempt),
                "receipt directory belongs to another attempt"
            );
            if receipt.state == CommandState::Started {
                receipt.state = CommandState::Uncertain;
                receipt.note=Some("service restarted after external start; inspect private history, never replay this command".into());
                atomic::write_json(&path, &receipt)?;
            }
        }
        let socket = endpoint.join("jj.sock");
        ensure!(
            socket.as_os_str().as_encoded_bytes().len() < 100,
            "command socket exceeds portable bound"
        );
        if socket.try_exists()? {
            ensure!(
                fs::symlink_metadata(&socket)?.file_type().is_socket(),
                "unexpected command endpoint file"
            );
            fs::remove_file(&socket)?;
        }
        let listener = UnixListener::bind(&socket)?;
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        let identity = CommandIdentity {
            format_version: 1,
            binding: CommandBinding::from(&config.view.attempt),
            token: format!(
                "{}{}",
                uuid::Uuid::new_v4().simple(),
                uuid::Uuid::new_v4().simple()
            ),
        };
        atomic::write_json(&endpoint.join("identity.json"), &identity)?;
        let inner = Arc::new(Inner {
            config,
            identity,
            socket,
            stop: AtomicBool::new(false),
            active: AtomicUsize::new(0),
            receipts: AtomicUsize::new(receipt_count),
            accept_done: AtomicBool::new(false),
            start_gate: Mutex::new(()),
            _owner: owner,
        });
        let running = inner.clone();
        thread::Builder::new()
            .name("rudder-jj-command-accept".into())
            .spawn(move || {
                while !running.stop.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            if running.active.fetch_add(1, Ordering::AcqRel) >= 8 {
                                running.active.fetch_sub(1, Ordering::AcqRel);
                                continue;
                            }
                            let worker = running.clone();
                            thread::spawn(move || {
                                serve(&worker, stream);
                                worker.active.fetch_sub(1, Ordering::AcqRel);
                            });
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5))
                        }
                        Err(_) => break,
                    }
                }
                let _ = fs::remove_file(&running.socket);
                running.accept_done.store(true, Ordering::Release);
            })?;
        Ok(Self { inner })
    }
    pub fn client_config(&self) -> CommandClientConfig {
        CommandClientConfig {
            format_version: 1,
            socket: self.inner.socket.clone(),
            identity_file: self.inner.socket.with_file_name("identity.json"),
            binding: self.inner.identity.binding.clone(),
        }
    }
    pub fn close(&self) {
        // Serialize closure with the actual spawn, not only request parsing.
        // After this returns no queued handler may start another CLI.
        if let Ok(_gate) = self.inner.start_gate.lock() {
            self.inner.stop.store(true, Ordering::Release);
        } else {
            self.inner.stop.store(true, Ordering::Release);
        }
    }
    pub fn await_closed(&self, timeout: Duration) -> Result<()> {
        self.close();
        let end = Instant::now() + timeout;
        while !self.inner.accept_done.load(Ordering::Acquire)
            || self.inner.active.load(Ordering::Acquire) != 0
        {
            ensure!(
                Instant::now() < end,
                "private VCS commands are still settling"
            );
            thread::sleep(Duration::from_millis(10));
        }
        Ok(())
    }
    pub fn quiesce(&self, timeout: Duration) -> Result<CommandFinalizationGuard> {
        let deadline = Instant::now() + timeout;
        self.await_closed(timeout)?;
        fence_private_writer(
            &self.inner.config,
            deadline.saturating_duration_since(Instant::now()),
        )
    }
}
impl Drop for CommandService {
    fn drop(&mut self) {
        self.close();
    }
}
fn serve(inner: &Inner, mut stream: UnixStream) {
    let result = (|| {
        stream.set_nonblocking(false)?;
        stream.set_read_timeout(Some(Duration::from_millis(20)))?;
        stream.set_write_timeout(Some(Duration::from_millis(20)))?;
        ensure!(
            same_peer_user(&stream),
            "command peer has another local identity"
        );
        let request: Request = serde_json::from_slice(&read_frame(
            &mut stream,
            MAX_REQUEST,
            Instant::now() + Duration::from_secs(2),
            || inner.stop.load(Ordering::Acquire),
        )?)?;
        ensure!(
            request.format_version == 1
                && request.identity.format_version == 1
                && request.identity.binding == inner.identity.binding,
            "command scope changed"
        );
        let a = request.identity.token.as_bytes();
        let b = inner.identity.token.as_bytes();
        ensure!(
            a.len() == b.len() && a.iter().zip(b).fold(0u8, |v, (a, b)| v | (a ^ b)) == 0,
            "command identity rejected"
        );
        ensure!(
            !inner.stop.load(Ordering::Acquire),
            "command service is closing"
        );
        provider(&inner.config.view)?.validate_authority(&inner.config.view.attempt)?;
        match request.operation {
            WireOperation::Inspect { command_id } => read_receipt(&inner.config, &command_id),
            WireOperation::Execute {
                command_id,
                command,
            } => execute(inner, &command_id, command),
        }
    })();
    let response = match result {
        Ok(receipt) => Response {
            format_version: 1,
            receipt: Some(receipt),
            error: None,
        },
        Err(error) => Response {
            format_version: 1,
            receipt: None,
            error: Some(format!("{error:#}")),
        },
    };
    let _ = write_frame(&mut stream, &response, MAX_RESPONSE);
}
fn receipt_path(config: &CommandServiceConfig, id: &str) -> Result<PathBuf> {
    let id = uuid::Uuid::parse_str(id)?;
    Ok(config
        .state_directory
        .join("receipts")
        .join(format!("{id}.json")))
}
fn read_receipt(config: &CommandServiceConfig, id: &str) -> Result<CommandReceipt> {
    let receipt: CommandReceipt = atomic::read_json(&receipt_path(config, id)?)?;
    ensure!(
        receipt.binding == CommandBinding::from(&config.view.attempt) && receipt.command_id == id,
        "receipt belongs to another attempt"
    );
    Ok(receipt)
}
fn execute(inner: &Inner, id: &str, command: SemanticCommand) -> Result<CommandReceipt> {
    let config = &inner.config;
    command.validate()?;
    let path = receipt_path(config, id)?;
    if path.try_exists()? {
        let receipt = read_receipt(config, id)?;
        ensure!(
            receipt.command == command,
            "command ID is already bound to another semantic request"
        );
        return Ok(receipt);
    }
    let writer = lock_file(&config.view.private_repository.join("rudder-command.lock"))?;
    writer
        .try_lock_exclusive()
        .context("private VCS command is busy")?;
    if path.try_exists()? {
        let receipt = read_receipt(config, id)?;
        ensure!(receipt.command == command, "command ID collision");
        return Ok(receipt);
    }
    let gate = config.state_directory.join("active-command.json");
    if gate.try_exists()? {
        let active: ActiveCommand = atomic::read_json(&gate)?;
        ensure!(
            active.format_version == 1
                && active.binding == CommandBinding::from(&config.view.attempt),
            "active command belongs to another attempt"
        );
        let old = read_receipt(config, &active.command_id)?;
        ensure!(
            old.state == CommandState::Completed,
            "a private VCS command is uncertain; explicit reconciliation is required"
        );
        fs::remove_file(&gate)?;
        File::open(&config.state_directory)?.sync_all()?;
    }
    ensure!(
        inner.receipts.load(Ordering::Acquire) < 4096,
        "private command receipt budget exhausted"
    );
    config.check_private_configuration()?;
    provider(&config.view)?.validate_authority(&config.view.attempt)?;
    let mut receipt = CommandReceipt {
        format_version: 1,
        command_id: id.into(),
        binding: CommandBinding::from(&config.view.attempt),
        command: command.clone(),
        state: CommandState::Started,
        before_heads: heads(config)?,
        after_heads: None,
        backend: if matches!(command, SemanticCommand::Edit { .. }) {
            "jj-lib-edit"
        } else {
            "jj-cli"
        }
        .into(),
        exit_code: None,
        stdout_hex: String::new(),
        stderr_hex: String::new(),
        output_truncated: false,
        note: None,
    };
    ensure!(
        receipt.before_heads.len() == 1,
        "private operation heads diverged; no automatic merge"
    );
    atomic::write_json(
        &gate,
        &ActiveCommand {
            format_version: 1,
            binding: receipt.binding.clone(),
            command_id: id.into(),
        },
    )?;
    atomic::write_json(&path, &receipt)?;
    inner.receipts.fetch_add(1, Ordering::AcqRel);
    let context = HostCommandContext {
        format_version: 1,
        service: config.clone(),
        command_id: id.into(),
        command,
    };
    let context_path = config.state_directory.join(format!("context-{id}.json"));
    atomic::write_json(&context_path, &context)?;
    let home = config.state_directory.join("home");
    vfs_provider::private_directory(&home)?;
    let empty_config = home.join("config.toml");
    if !empty_config.try_exists()? {
        atomic::write_bytes_bounded(&empty_config, b"", 1024)?;
    }
    ensure!(
        read_private_file(&empty_config, 1024)?.is_empty(),
        "managed user configuration must remain empty"
    );
    let inherited = unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_DUPFD, 3) };
    ensure!(inherited >= 3, "cannot retain private writer ownership");
    let inherited = unsafe { std::os::fd::OwnedFd::from_raw_fd(inherited) };
    let mut child = Command::new(&config.binary);
    child
        .args(context.argv())
        .current_dir(&config.view.private_repository)
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .env("LANG", "C.UTF-8")
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &home)
        .env("JJ_CONFIG", &empty_config)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env(HOST_CONTEXT_ENV, &context_path)
        .env(INHERITED_LOCK_ENV, inherited.as_raw_fd().to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let expected_parent = std::process::id();
    unsafe {
        child.pre_exec(move || {
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            #[cfg(target_os = "linux")]
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0
                || libc::getppid() as u32 != expected_parent
            {
                return Err(std::io::Error::other(
                    "managed CLI lost its owning service process",
                ));
            }
            #[cfg(not(target_os = "linux"))]
            let _ = expected_parent;
            Ok(())
        });
    }
    let run = (|| {
        let start = inner
            .start_gate
            .lock()
            .map_err(|_| anyhow::anyhow!("CLI start gate poisoned"))?;
        ensure!(
            !inner.stop.load(Ordering::Acquire),
            "command service closed before external start"
        );
        let mut child = OwnedCommandChild {
            child: child.spawn()?,
            reaped: false,
        };
        drop(start);
        drop(inherited);
        let stdout = child.child.stdout.take().unwrap();
        let stderr = child.child.stderr.take().unwrap();
        let deadline = Instant::now() + Duration::from_millis(config.command_timeout_ms);
        let drain_deadline = deadline + Duration::from_secs(2);
        let finished = Arc::new(Mutex::new(None));
        let (out_done, err_done) = (finished.clone(), finished.clone());
        let out = thread::spawn(move || collect_output(stdout, drain_deadline, out_done));
        let err = thread::spawn(move || collect_output(stderr, drain_deadline, err_done));
        let mut timed_out = false;
        let mut termination = None;
        let status = (|| -> Result<std::process::ExitStatus> {
            loop {
                if let Some(status) = child.child.try_wait()? {
                    child.reaped = true;
                    break Ok(status);
                }
                if (Instant::now() >= deadline || inner.stop.load(Ordering::Acquire))
                    && termination.is_none()
                {
                    timed_out = true;
                    unsafe {
                        libc::kill(-(child.child.id() as i32), libc::SIGKILL);
                    }
                    termination = Some(Instant::now() + Duration::from_secs(2));
                }
                ensure!(
                    termination.is_none_or(|end| Instant::now() < end),
                    "owned CLI did not exit after termination; writer fence remains authoritative"
                );
                thread::sleep(Duration::from_millis(10));
            }
        })();
        if let Ok(mut done) = finished.lock() {
            *done = Some(Instant::now());
        }
        let (stdout, out_truncated) = out
            .join()
            .map_err(|_| anyhow::anyhow!("stdout reader failed"))??;
        let (stderr, err_truncated) = err
            .join()
            .map_err(|_| anyhow::anyhow!("stderr reader failed"))??;
        let status = status?;
        Ok::<_, anyhow::Error>((
            status.code(),
            stdout,
            stderr,
            out_truncated || err_truncated,
            timed_out,
        ))
    })();
    match run {
        Ok((code, out, err, truncated, timed_out)) => {
            receipt.state = if timed_out {
                CommandState::Uncertain
            } else {
                CommandState::Completed
            };
            receipt.exit_code = code;
            receipt.stdout_hex = hex::encode(out);
            receipt.stderr_hex = hex::encode(err);
            receipt.output_truncated = truncated;
            if timed_out {
                receipt.note = Some(
                    "CLI timed out or was stopped after possible private history effects; never replay".into(),
                );
            }
        }
        Err(error) => {
            receipt.state = CommandState::Uncertain;
            receipt.note = Some(format!("command outcome needs reconciliation: {error:#}"));
        }
    }
    receipt.after_heads = heads(config).ok();
    if receipt.exit_code != Some(0)
        && (receipt
            .after_heads
            .as_ref()
            .is_none_or(|after| after != &receipt.before_heads)
            || config
                .view
                .projection_state
                .join("adoption-pending.json")
                .try_exists()?
            || config
                .view
                .projection_state
                .join("capture-pending.json")
                .try_exists()?)
    {
        receipt.state = CommandState::Uncertain;
        receipt.note=Some("command returned without successful confirmation after possible private history or observation effects; inspect without replay".into());
    }
    atomic::write_json(&path, &receipt)?;
    if receipt.state == CommandState::Completed {
        fs::remove_file(gate)?;
        File::open(&config.state_directory)?.sync_all()?;
    }
    let _ = fs::remove_file(context_path);
    Ok(receipt)
}
struct OwnedCommandChild {
    child: std::process::Child,
    reaped: bool,
}
impl Drop for OwnedCommandChild {
    fn drop(&mut self) {
        if self.reaped {
            return;
        }
        if self.child.try_wait().ok().flatten().is_some() {
            self.reaped = true;
            return;
        }
        // The unreaped owned child pins its process identity. Never signal a
        // persisted PID or a group after its root child has been reaped.
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if self.child.try_wait().ok().flatten().is_some() {
                self.reaped = true;
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        // Uninterruptible kernel I/O can outlive this bound. Its inherited
        // writer FD prevents a finalization guard/checkpoint from succeeding.
    }
}

fn collect_output(
    mut read: impl Read + AsRawFd,
    deadline: Instant,
    finished: Arc<Mutex<Option<Instant>>>,
) -> Result<(Vec<u8>, bool)> {
    let fd = read.as_raw_fd();
    ensure!(
        unsafe { libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK) } >= 0,
        "cannot bound command output reader"
    );
    let mut out = Vec::new();
    let mut buffer = [0; 16 * 1024];
    let mut truncated = false;
    loop {
        let tail_done = finished
            .lock()
            .map_err(|_| anyhow::anyhow!("output completion state poisoned"))?
            .is_some_and(|at| at.elapsed() >= Duration::from_secs(2));
        if Instant::now() >= deadline || tail_done {
            return Ok((out, true));
        }
        match read.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => {
                let keep = (OUTPUT_LIMIT - out.len()).min(n);
                out.extend_from_slice(&buffer[..keep]);
                truncated |= keep != n;
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) =>
            {
                if Instant::now() >= deadline {
                    return Ok((out, true));
                }
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok((out, truncated))
}
pub(crate) fn lock_file(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o777 == 0o600
            && metadata.nlink() == 1,
        "unsafe private VCS lock file"
    );
    Ok(file)
}

pub struct CommandClient {
    config: CommandClientConfig,
}
impl CommandClient {
    pub fn new(config: CommandClientConfig) -> Result<Self> {
        ensure!(config.format_version == 1, "unsupported command client");
        let identity: CommandIdentity =
            serde_json::from_slice(&read_private_file(&config.identity_file, 32 * 1024)?)?;
        ensure!(
            identity.binding == config.binding,
            "command identity scope differs"
        );
        Ok(Self { config })
    }
    pub fn execute(&self, id: &str, command: SemanticCommand) -> Result<CommandReceipt> {
        command.validate()?;
        self.call(WireOperation::Execute {
            command_id: id.into(),
            command,
        })
    }
    pub fn inspect(&self, id: &str) -> Result<CommandReceipt> {
        self.call(WireOperation::Inspect {
            command_id: id.into(),
        })
    }
    fn call(&self, operation: WireOperation) -> Result<CommandReceipt> {
        let identity: CommandIdentity =
            serde_json::from_slice(&read_private_file(&self.config.identity_file, 32 * 1024)?)?;
        ensure!(
            identity.binding == self.config.binding,
            "command attempt was replaced"
        );
        let mut stream = UnixStream::connect(&self.config.socket)?;
        stream.set_read_timeout(Some(Duration::from_millis(20)))?;
        stream.set_write_timeout(Some(Duration::from_millis(20)))?;
        write_frame(
            &mut stream,
            &Request {
                format_version: 1,
                identity,
                operation,
            },
            MAX_REQUEST,
        )?;
        let response: Response = serde_json::from_slice(&read_frame(
            &mut stream,
            MAX_RESPONSE,
            Instant::now() + Duration::from_secs(125),
            || false,
        )?)?;
        ensure!(
            response.format_version == 1 && response.receipt.is_some() != response.error.is_some(),
            "invalid command result"
        );
        if let Some(error) = response.error {
            bail!("{error}")
        }
        let receipt = response.receipt.unwrap();
        ensure!(
            receipt.binding == self.config.binding,
            "command result scope differs"
        );
        Ok(receipt)
    }
}
pub fn run_guest_cli(config_path: &Path, args: &[String]) -> Result<u8> {
    let diagnostic = match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["--version"] | ["-V"] => Some(false),
        ["--help"] | ["-h"] => Some(true),
        _ => None,
    };
    if let Some(help) = diagnostic {
        // Role is checked by the executable before reaching this entrypoint.
        // Read only this guest configuration; no identity, broker connection,
        // observation, private history or command receipt is needed.
        let config: CommandClientConfig =
            serde_json::from_slice(&read_private_file(config_path, 32 * 1024)?)?;
        ensure!(
            config.format_version == 1
                && config.socket.is_absolute()
                && config.identity_file.is_absolute(),
            "invalid scoped jj diagnostic configuration"
        );
        println!("rudder-jj {ADAPTER_VERSION}");
        if help {
            print!("{GUEST_HELP}");
        }
        return Ok(0);
    }
    let command = parse_guest_arguments(args)?;
    let config: CommandClientConfig =
        serde_json::from_slice(&read_private_file(config_path, 32 * 1024)?)?;
    let id = uuid::Uuid::new_v4().to_string();
    let receipt = CommandClient::new(config)?.execute(&id, command)?;
    std::io::stdout().write_all(&hex::decode(receipt.stdout_hex)?)?;
    std::io::stderr().write_all(&hex::decode(receipt.stderr_hex)?)?;
    if receipt.output_truncated {
        writeln!(
            std::io::stderr(),
            "Managed jj output was truncated at its configured limit."
        )?;
    }
    ensure!(
        receipt.state == CommandState::Completed,
        "private VCS command {} is uncertain; inspect it before another command",
        id
    );
    Ok(receipt.exit_code.unwrap_or(1).clamp(0, 255) as u8)
}
fn read_private_file(path: &Path, maximum: u64) -> Result<Vec<u8>> {
    let file = atomic::open_read(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o777 == 0o600
            && metadata.nlink() == 1
            && metadata.len() <= maximum,
        "unsafe protected command configuration"
    );
    let mut bytes = Vec::new();
    file.take(maximum + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= maximum,
        "command configuration exceeds bound"
    );
    Ok(bytes)
}
fn read_private_or_git_file(path: &Path, maximum: u64) -> Result<Vec<u8>> {
    let file = atomic::open_read(path)?;
    ensure!(
        file.metadata()?.len() <= maximum,
        "Git config exceeds bound"
    );
    let mut bytes = Vec::new();
    file.take(maximum + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= maximum, "Git config exceeds bound");
    Ok(bytes)
}
fn read_frame(
    stream: &mut UnixStream,
    maximum: usize,
    deadline: Instant,
    cancel: impl Fn() -> bool,
) -> Result<Vec<u8>> {
    let exact = |stream: &mut UnixStream, output: &mut [u8]| -> Result<()> {
        let mut offset = 0;
        while offset < output.len() {
            ensure!(
                !cancel() && Instant::now() < deadline,
                "command frame deadline or service closure"
            );
            match stream.read(&mut output[offset..]) {
                Ok(0) => bail!("command connection ended without confirmation"),
                Ok(count) => offset += count,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock
                            | std::io::ErrorKind::TimedOut
                            | std::io::ErrorKind::Interrupted
                    ) => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    };
    let mut prefix = [0; 4];
    exact(stream, &mut prefix)?;
    let size = u32::from_be_bytes(prefix) as usize;
    ensure!(size > 0 && size <= maximum, "command frame exceeds bound");
    let mut out = vec![0; size];
    exact(stream, &mut out)?;
    Ok(out)
}
fn write_frame(stream: &mut UnixStream, value: &impl Serialize, maximum: usize) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(bytes.len() <= maximum, "command result exceeds bound");
    let deadline = Instant::now() + Duration::from_secs(2);
    for part in [&(bytes.len() as u32).to_be_bytes()[..], bytes.as_slice()] {
        let mut offset = 0;
        while offset < part.len() {
            ensure!(Instant::now() < deadline, "command response deadline");
            match stream.write(&part[offset..]) {
                Ok(0) => bail!("command peer stopped accepting results"),
                Ok(count) => offset += count,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock
                            | std::io::ErrorKind::TimedOut
                            | std::io::ErrorKind::Interrupted
                    ) => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    Ok(())
}

fn same_peer_user(stream: &UnixStream) -> bool {
    #[cfg(target_os = "linux")]
    {
        let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
        let mut length = std::mem::size_of_val(&credentials) as libc::socklen_t;
        (unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut credentials as *mut libc::ucred).cast(),
                &mut length,
            )
        }) == 0
            && credentials.uid == unsafe { libc::geteuid() }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let (mut uid, mut gid) = (0, 0);
        (unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) }) == 0
            && uid == unsafe { libc::geteuid() }
    }
}
