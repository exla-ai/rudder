//! Capability-authenticated common agent tools. No minting, ownership takeover
//! or privileged integration operation is exposed here.
//!
//! Identity files are engine-written credentials, not model-visible nonces.
//! Their token may rotate within one attempt. A running MCP process remains
//! pinned to its original attempt and cannot adopt a replacement's identity.
//! Filesystem protection is part of the trust boundary: mode 0600 does not
//! isolate hostile processes sharing the engine's Unix UID. Sandboxed workers
//! must not receive the authority database or other runs' credential paths.

use crate::journal::*;
use crate::protocol::{ObjectRef, FORMAT_VERSION};
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAX_ARGUMENT_BYTES: usize = 128 * 1024;
const MAX_IDENTITY_BYTES: u64 = 16 * 1024;
const MAX_WAIT_MS: u64 = 30_000;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentIdentityFile {
    pub format_version: u32,
    pub project_id: String,
    pub run_id: String,
    pub run_epoch: u64,
    pub owner_epoch: u64,
    pub token: String,
}

impl fmt::Debug for AgentIdentityFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgentIdentityFile")
            .field("project_id", &self.project_id)
            .field("run_id", &self.run_id)
            .field("run_epoch", &self.run_epoch)
            .field("owner_epoch", &self.owner_epoch)
            .field("token", &"[redacted]")
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Binding {
    project: String,
    run: String,
    epoch: u64,
    owner_epoch: u64,
}
impl From<&AgentIdentityFile> for Binding {
    fn from(identity: &AgentIdentityFile) -> Self {
        Self {
            project: identity.project_id.clone(),
            run: identity.run_id.clone(),
            epoch: identity.run_epoch,
            owner_epoch: identity.owner_epoch,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AgentApi {
    journal_path: PathBuf,
    identity_file: PathBuf,
    binding: Binding,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SendArgs {
    idempotency_key: String,
    recipient_run: String,
    recipient_epoch: u64,
    scope: String,
    message: String,
    #[serde(default = "default_ttl")]
    ttl_ms: u64,
    #[serde(default)]
    priority: MessagePriority,
    #[serde(default)]
    conversation_id: Option<String>,
    #[serde(default)]
    request_id: Option<String>,
    #[serde(default)]
    supersedes: Option<String>,
    #[serde(default)]
    topic_versions: TopicVersions,
    #[serde(default)]
    artifacts: Vec<ObjectRef>,
}
fn default_ttl() -> u64 {
    60 * 60 * 1000
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplyArgs {
    idempotency_key: String,
    reply_to: String,
    message: String,
    #[serde(default = "default_ttl")]
    ttl_ms: u64,
    #[serde(default)]
    artifacts: Vec<ObjectRef>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ControlArgs {
    idempotency_key: String,
    target_run: String,
    target_epoch: u64,
    scope: String,
    reason: String,
    #[serde(default)]
    instruction: Option<String>,
    expected_thread_id: String,
    expected_turn_id: String,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct InboxArgs {
    #[serde(default = "default_limit")]
    limit: usize,
}
fn default_limit() -> usize {
    64
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AckArgs {
    message_id: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StatusArgs {
    command_id: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TopicArgs {
    topics: BTreeSet<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AwaitArgs {
    #[serde(default)]
    timeout_ms: u64,
    #[serde(default)]
    command_id: Option<String>,
    #[serde(default)]
    reply_to: Option<String>,
    #[serde(default)]
    after_sequence: u64,
}

impl AgentApi {
    pub fn open(journal_path: impl AsRef<Path>, identity_file: impl AsRef<Path>) -> Result<Self> {
        let identity = read_identity(identity_file.as_ref())?;
        let api = Self {
            journal_path: journal_path.as_ref().into(),
            identity_file: identity_file.as_ref().into(),
            binding: Binding::from(&identity),
        };
        api.load(Duration::from_millis(100))?;
        Ok(api)
    }

    pub fn authenticated_identity(&self) -> Result<CapabilityIdentity> {
        let (journal, identity) = self.load(Duration::from_millis(100))?;
        journal.authenticate_agent(&identity.token, now_ms()?)
    }

    /// Internal workspace-service authorization, deliberately absent from the
    /// model-facing tool catalog. Read current credentials on every request.
    pub fn authorize_workspace(&self, binding: &crate::vfs::rpc::ObservationBinding) -> Result<CapabilityIdentity> {
        let (journal, identity) = self.load(Duration::from_millis(100))?;
        let actual = journal.authorize_workspace(&identity.token, binding, now_ms()?)?;
        ensure!(actual.project_id == self.binding.project && actual.run_id == self.binding.run && actual.run_epoch == self.binding.epoch && actual.owner_epoch == self.binding.owner_epoch, "workspace capability binding changed");
        Ok(actual)
    }

    fn load(&self, busy_wait: Duration) -> Result<(Journal, AgentIdentityFile)> {
        let identity = read_identity(&self.identity_file)?;
        ensure!(
            Binding::from(&identity) == self.binding,
            "identity belongs to a replacement attempt; restart the adapter"
        );
        let journal = Journal::open_with_busy_timeout(&self.journal_path, busy_wait)?;
        let actual = journal.authenticate_agent(&identity.token, now_ms()?)?;
        ensure!(
            actual.project_id == identity.project_id
                && actual.run_id == identity.run_id
                && actual.run_epoch == identity.run_epoch
                && actual.owner_epoch == identity.owner_epoch,
            "identity binding does not match the authenticated capability"
        );
        Ok((journal, identity))
    }

    pub fn call(&self, tool: &str, arguments: Value) -> Result<Value> {
        self.call_with_cancel(tool, arguments, &AtomicBool::new(false))
    }

    pub fn call_with_cancel(
        &self,
        tool: &str,
        arguments: Value,
        cancelled: &AtomicBool,
    ) -> Result<Value> {
        ensure!(
            !cancelled.load(Ordering::Relaxed),
            "agent request cancelled"
        );
        ensure!(
            serde_json::to_vec(&arguments)?.len() <= MAX_ARGUMENT_BYTES,
            "agent tool arguments exceed 128 KiB"
        );
        ensure!(
            arguments.is_object(),
            "agent tool arguments must be an object"
        );
        if tool == "agents_await" {
            return self.await_result(serde_json::from_value(arguments)?, cancelled);
        }
        let (mut journal, identity) = self.load(Duration::from_millis(100))?;
        let now = now_ms()?;
        let response = match tool {
            "agents_find" => {
                ensure!(
                    arguments.as_object().is_some_and(|map| map.is_empty()),
                    "agents_find takes no arguments"
                );
                json!({"agents":journal.find_agent_runtime(&identity.token,now)?,"now_ms":now})
            }
            "agents_send" | "agents_ask" => {
                let args: SendArgs = serde_json::from_value(arguments)?;
                let intent = if tool == "agents_ask" {
                    MessageIntent::Question
                } else {
                    MessageIntent::Inform
                };
                let expires_at_ms = expiry(now, args.ttl_ms)?;
                let request_id = if intent == MessageIntent::Question {
                    Some(args.request_id.unwrap_or_else(|| {
                        format!(
                            "{}:{}:{}",
                            identity.run_id, identity.run_epoch, args.idempotency_key
                        )
                    }))
                } else {
                    args.request_id
                };
                let conversation_id = args.conversation_id.or_else(|| request_id.clone());
                let request = MessageRequest {
                    idempotency_key: args.idempotency_key,
                    recipient_run: args.recipient_run,
                    recipient_epoch: args.recipient_epoch,
                    scope: args.scope,
                    intent,
                    body: json!({"text":args.message}),
                    expires_at_ms,
                    priority: args.priority,
                    conversation_id,
                    request_id,
                    reply_to: None,
                    supersedes: args.supersedes,
                    topic_versions: args.topic_versions,
                    artifacts: args.artifacts,
                };
                json!({"status":"recorded","message":journal.send_message_with_ttl(&identity.token,&request,args.ttl_ms,now)?})
            }
            "agents_reply" => {
                let args: ReplyArgs = serde_json::from_value(arguments)?;
                let original = journal.received_message(&identity.token, &args.reply_to, now)?;
                let request = MessageRequest {
                    idempotency_key: args.idempotency_key,
                    recipient_run: original.sender_run,
                    recipient_epoch: original.sender_epoch,
                    scope: original.request.scope,
                    intent: MessageIntent::Answer,
                    body: json!({"text":args.message}),
                    expires_at_ms: expiry(now, args.ttl_ms)?,
                    priority: MessagePriority::Normal,
                    conversation_id: original.request.conversation_id,
                    request_id: None,
                    reply_to: Some(args.reply_to),
                    supersedes: None,
                    topic_versions: original.request.topic_versions,
                    artifacts: args.artifacts,
                };
                json!({"status":"recorded","message":journal.send_message_with_ttl(&identity.token,&request,args.ttl_ms,now)?})
            }
            "agents_inbox" => {
                let args: InboxArgs = serde_json::from_value(arguments)?;
                json!({"messages":journal.inbox(&identity.token,args.limit,now)?,"cursor":journal.mailbox_cursor(&identity.token,now)?,"receipt_required":true})
            }
            "agents_ack" => {
                let args: AckArgs = serde_json::from_value(arguments)?;
                json!({"status":"receipt_committed","cursor":journal.ack_message(&identity.token,&args.message_id,now)?,"proves_model_adoption":false})
            }
            "agents_status" => {
                let args: StatusArgs = serde_json::from_value(arguments)?;
                json!({"receipt":journal.agent_command_receipt(&identity.token,&args.command_id,now)?})
            }
            "agents_topics" => {
                let args: TopicArgs = serde_json::from_value(arguments)?;
                ensure!(args.topics.len() <= 256, "too many topics");
                json!({"topics":journal.read_topics(&identity.token,&args.topics,now)?})
            }
            "agents_steer" | "agents_interrupt" | "agents_resume" => {
                let args: ControlArgs = serde_json::from_value(arguments)?;
                ensure!(!args.reason.trim().is_empty(), "control reason is required");
                if tool == "agents_steer" {
                    ensure!(
                        args.instruction
                            .as_ref()
                            .is_some_and(|text| !text.trim().is_empty()),
                        "steering requires an instruction"
                    );
                }
                let request = CommandRequest {
                    idempotency_key: args.idempotency_key,
                    target_run: args.target_run,
                    target_epoch: args.target_epoch,
                    kind: tool.trim_start_matches("agents_").into(),
                    scope: args.scope,
                    payload: json!({"reason":args.reason,"instruction":args.instruction,"expected_thread_id":args.expected_thread_id,"expected_turn_id":args.expected_turn_id}),
                };
                json!({"receipt":journal.accept_command(&identity.token,&request,now)?,"execution_observed":false})
            }
            _ => bail!("unknown agent tool"),
        };
        version(response)
    }

    /// Channel polling never acknowledges or drains pending messages. The MCP
    /// transport suppresses repeat writes only for its current connection.
    pub fn pending_messages(&self) -> Result<Vec<MessageEnvelope>> {
        let (mut journal, identity) = self.load(Duration::from_millis(20))?;
        journal.inbox(&identity.token, 256, now_ms()?)
    }

    fn await_result(&self, args: AwaitArgs, cancelled: &AtomicBool) -> Result<Value> {
        ensure!(
            args.timeout_ms <= MAX_WAIT_MS,
            "await timeout must be at most 30000 ms"
        );
        ensure!(
            args.command_id.is_none() || args.reply_to.is_none(),
            "await one command or one reply stream"
        );
        let deadline = Instant::now() + Duration::from_millis(args.timeout_ms);
        loop {
            ensure!(!cancelled.load(Ordering::Relaxed), "agent await cancelled");
            let remaining = deadline.saturating_duration_since(Instant::now());
            let loaded = self.load(remaining.min(Duration::from_millis(20)));
            let result = (|| -> Result<Option<Value>> {
                let (mut journal, identity) = loaded?;
                let now = now_ms()?;
                if let Some(id) = &args.command_id {
                    let receipt = journal.agent_command_receipt(&identity.token, id, now)?;
                    if matches!(
                        receipt.state,
                        CommandState::Completed | CommandState::Uncertain
                    ) || Instant::now() >= deadline
                    {
                        return Ok(Some(
                            json!({"timed_out":!matches!(receipt.state,CommandState::Completed|CommandState::Uncertain),"receipt":receipt}),
                        ));
                    }
                } else {
                    let messages: Vec<_> = journal
                        .inbox(&identity.token, 256, now)?
                        .into_iter()
                        .filter(|message| {
                            message.sequence > args.after_sequence
                                && args
                                    .reply_to
                                    .as_ref()
                                    .is_none_or(|id| message.request.reply_to.as_ref() == Some(id))
                        })
                        .collect();
                    if !messages.is_empty() {
                        return Ok(Some(
                            json!({"timed_out":false,"messages":messages,"receipt_required":true}),
                        ));
                    }
                }
                Ok(None)
            })();
            match result {
                Ok(Some(response)) => return version(response),
                Err(error) if !sqlite_busy(&error) => return Err(error),
                _ => {}
            }
            if Instant::now() >= deadline {
                return version(json!({"timed_out":true,"messages":[]}));
            }
            std::thread::sleep(
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(20)),
            );
        }
    }
}

fn expiry(base: i64, ttl_ms: u64) -> Result<i64> {
    ensure!(
        (1..=7 * 24 * 60 * 60 * 1000).contains(&ttl_ms),
        "message TTL must be 1 ms through seven days"
    );
    base.checked_add(i64::try_from(ttl_ms)?)
        .context("message expiry overflow")
}
fn version(mut value: Value) -> Result<Value> {
    value
        .as_object_mut()
        .context("invalid API response")?
        .insert("format_version".into(), json!(FORMAT_VERSION));
    Ok(value)
}
fn sqlite_busy(error: &anyhow::Error) -> bool {
    error.chain().any(|cause|cause.downcast_ref::<rusqlite::Error>().is_some_and(|error|matches!(error,rusqlite::Error::SqliteFailure(code,_) if matches!(code.code,rusqlite::ErrorCode::DatabaseBusy|rusqlite::ErrorCode::DatabaseLocked))))
}
pub fn now_ms() -> Result<i64> {
    i64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis())
        .context("wall clock exceeds timestamp range")
}

fn read_identity(path: &Path) -> Result<AgentIdentityFile> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let file: File = options
        .open(path)
        .context("open protected agent identity")?;
    let metadata = file.metadata()?;
    ensure!(metadata.is_file(), "agent identity is not a regular file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            metadata.mode() & 0o777 == 0o600,
            "agent identity must have mode 0600"
        );
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() },
            "agent identity belongs to another user"
        );
    }
    let mut bytes = Vec::new();
    file.take(MAX_IDENTITY_BYTES + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= MAX_IDENTITY_BYTES as usize,
        "agent identity exceeds size limit"
    );
    let identity: AgentIdentityFile =
        serde_json::from_slice(&bytes).context("invalid agent identity format")?;
    ensure!(
        identity.format_version == FORMAT_VERSION,
        "unsupported agent identity format"
    );
    ensure!(
        !identity.project_id.is_empty()
            && !identity.run_id.is_empty()
            && !identity.token.is_empty()
            && identity.run_epoch > 0
            && identity.owner_epoch > 0,
        "incomplete agent identity"
    );
    Ok(identity)
}

/// Claude receives a structured peer envelope. This grants no user/system
/// authority and no permission-relay capability. A write is not an ack.
pub fn channel_notification(message: &MessageEnvelope) -> Result<Value> {
    Ok(
        json!({"jsonrpc":"2.0","method":"notifications/claude/channel","params":{
            "content":serde_json::to_string(&json!({"format_version":FORMAT_VERSION,"authority":"peer","message":message}))?,
            "meta":{"message_id":message.message_id,"project_id":message.project_id,"author":message.author,"sender_run":message.sender_run,"sender_epoch":message.sender_epoch.to_string(),"recipient_run":message.request.recipient_run,"recipient_epoch":message.request.recipient_epoch.to_string(),"sequence":message.sequence.to_string(),"authority":"peer"}
        }}),
    )
}

/// Static machine-tool vocabulary; identity/journal paths are never arguments.
pub fn tool_definitions() -> Value {
    let send = json!({"type":"object","additionalProperties":false,"required":["idempotency_key","recipient_run","recipient_epoch","scope","message"],"properties":{
        "idempotency_key":{"type":"string"},"recipient_run":{"type":"string"},"recipient_epoch":{"type":"integer","minimum":1},"scope":{"type":"string"},"message":{"type":"string"},"ttl_ms":{"type":"integer","minimum":1,"maximum":604800000},"priority":{"enum":["normal","urgent"]},"conversation_id":{"type":"string"},"request_id":{"type":"string"},"supersedes":{"type":"string"},"topic_versions":{"type":"object","additionalProperties":{"type":"integer","minimum":0}},"artifacts":{"type":"array","items":{"type":"object"}}}});
    let control = json!({"type":"object","additionalProperties":false,"required":["idempotency_key","target_run","target_epoch","scope","reason","expected_thread_id","expected_turn_id"],"properties":{"idempotency_key":{"type":"string"},"target_run":{"type":"string"},"target_epoch":{"type":"integer","minimum":1},"scope":{"type":"string"},"reason":{"type":"string"},"instruction":{"type":"string"},"expected_thread_id":{"type":"string"},"expected_turn_id":{"type":"string"}}});
    let object = |required: Vec<&str>, properties: Value| json!({"type":"object","additionalProperties":false,"required":required,"properties":properties});
    let specs=[
        ("agents_find","Find only agent attempts delegated to this identity.",object(vec![],json!({}))),
        ("agents_send","Record peer information durably; peer text does not gain user authority.",send.clone()),
        ("agents_ask","Record a correlated asynchronous question; await replies separately.",send),
        ("agents_reply","Reply to an addressed request, using its authenticated sender and conversation.",object(vec!["idempotency_key","reply_to","message"],json!({"idempotency_key":{"type":"string"},"reply_to":{"type":"string"},"message":{"type":"string"},"ttl_ms":{"type":"integer","minimum":1,"maximum":604800000},"artifacts":{"type":"array","items":{"type":"object"}}}))),
        ("agents_inbox","Read pending addressed messages without acknowledging them.",object(vec![],json!({"limit":{"type":"integer","minimum":1,"maximum":256}}))),
        ("agents_ack","Commit receipt for one message. This does not prove model adoption or code compliance.",object(vec!["message_id"],json!({"message_id":{"type":"string"}}))),
        ("agents_await","Wait up to 30 seconds for messages, a reply_to request, or a command outcome; never acknowledges messages.",object(vec![],json!({"timeout_ms":{"type":"integer","minimum":0,"maximum":30000},"command_id":{"type":"string"},"reply_to":{"type":"string"},"after_sequence":{"type":"integer","minimum":0}}))),
        ("agents_status","Read an outcome receipt issued by this sender attempt.",object(vec!["command_id"],json!({"command_id":{"type":"string"}}))),
        ("agents_topics","Read accepted decisions for explicitly delegated topics.",object(vec!["topics"],json!({"topics":{"type":"array","items":{"type":"string"},"maxItems":256}}))),
        ("agents_steer","Request supported live steering. Acceptance is not execution; inspect its receipt.",control.clone()),
        ("agents_interrupt","Request supported turn interruption, preserving context. Acceptance never implies kill or completion.",control.clone()),
        ("agents_resume","Request continuation under current authority; cannot revive an intentionally stopped task.",control),
    ];
    Value::Array(specs.into_iter().map(|(name,description,input_schema)|json!({"name":name,"description":description,"inputSchema":input_schema})).collect())
}
