//! One manifest shared with the CLI. Parsing a surface request grants no engine
//! authority. The App owner authenticates and dispatches existing typed APIs.
use anyhow::{bail, ensure, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::OnceLock;

pub const REGISTRY_JSON: &str = include_str!("../../../src/operations/registry.json");

#[derive(Debug, Deserialize)]
pub struct Family { pub name: String, pub summary: String }
#[derive(Debug, Deserialize)]
pub struct Operation {
    pub id: String, pub cli: Vec<String>, pub adapter: Option<String>,
    pub scope: String, pub capability: String, pub arguments: Value,
    pub receipt: String, pub tool: Option<String>,
}
#[derive(Debug, Deserialize)]
pub struct Alias { pub name: String, pub replacement: String, pub owner: String, pub expires: String }
#[derive(Debug, Deserialize)]
pub struct Registry {
    pub format_version: u32, pub families: Vec<Family>, pub operations: Vec<Operation>,
    pub slash: Vec<String>, pub aliases: Vec<Alias>, pub slash_aliases: Vec<Alias>,
    pub callbacks: Vec<String>, pub retired: Vec<String>, pub retired_slash: Vec<String>,
    pub flag_aliases: Vec<Alias>,
}
pub fn registry() -> &'static Registry {
    static VALUE: OnceLock<Registry> = OnceLock::new();
    VALUE.get_or_init(|| serde_json::from_str(REGISTRY_JSON).expect("checked command registry"))
}
pub fn operation(id: &str) -> Result<&'static Operation> {
    registry().operations.iter().find(|entry| entry.id == id)
        .ok_or_else(|| anyhow::anyhow!("Unknown operation: {id}"))
}
pub fn completions(words: &[&str]) -> Vec<String> {
    if words.is_empty() { return registry().families.iter().map(|entry| entry.name.clone()).collect(); }
    let mut result = Vec::new();
    for entry in &registry().operations {
        if words.iter().enumerate().all(|(index, word)| entry.cli.get(index).is_some_and(|value| value == word)) {
            if let Some(next) = entry.cli.get(words.len()) { if !result.contains(next) { result.push(next.clone()); } }
        }
    }
    result
}
#[derive(Debug, PartialEq, Eq)]
pub enum ComposerInput { Text, Backend, Rudder(String), Refused(String) }
/// Call only in the correct composer. Backend slash text is forwarded intact
/// before interpreting Rudder's namespace, including retired Rudder spellings.
/// `utc_date` is the owner's trusted YYYY-MM-DD date, never agent input.
pub fn classify_composer(text: &str, backend_composer: bool, utc_date: &str) -> ComposerInput {
    let Some(command) = text.trim().split_whitespace().next().filter(|word| word.starts_with('/')) else { return ComposerInput::Text; };
    if backend_composer { return ComposerInput::Backend; }
    let r = registry();
    if r.retired_slash.iter().any(|value| value == command) {
        return ComposerInput::Refused(format!("{command} is retired; use ordinary task input or a selected agent action"));
    }
    if r.slash.iter().any(|value| value == command) { return ComposerInput::Rudder(command.into()); }
    if let Some(alias) = r.slash_aliases.iter().find(|value| value.name == command) {
        if utc_date.len() != 10 || utc_date >= alias.expires.as_str() {
            return ComposerInput::Refused(format!("{command} expired on {}; use {}", alias.expires, alias.replacement));
        }
        return ComposerInput::Rudder(command.into());
    }
    ComposerInput::Refused(format!("Unknown Rudder command {command}; use the backend composer for backend slash commands"))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentScope { pub run_id: String, pub run_epoch: u64 }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all="snake_case")]
pub enum Location { Local, Cloud }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all="snake_case")]
pub enum Checkout { Main, Isolated }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all="snake_case")]
pub enum Selection { Project, Agent, Group, Change, Conversation, Account }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionScope {
    pub project_id: String, pub location: Location, pub checkout: Checkout, pub owner_epoch: u64,
    pub agents: Vec<AgentScope>, pub selection: Selection,
    #[serde(skip_serializing_if="Option::is_none")] pub home_id: Option<String>,
    #[serde(skip_serializing_if="Option::is_none")] pub communication_scope: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SurfaceRequest {
    pub format_version: u32, pub operation: String, pub idempotency_key: String,
    pub scope: ExecutionScope, pub arguments: Value,
}
fn text(value: &Value, maximum: usize) -> Result<()> {
    let Some(value) = value.as_str() else { bail!("Expected bounded text"); };
    ensure!(!value.trim().is_empty() && value.len() <= maximum && !value.contains('\0'), "Invalid bounded text"); Ok(())
}
fn fields(args: &Value, allowed: &[&str]) -> Result<()> {
    let map = args.as_object().ok_or_else(|| anyhow::anyhow!("Expected arguments object"))?;
    ensure!(map.keys().all(|key| allowed.contains(&key.as_str())), "Unknown operation field"); Ok(())
}
impl SurfaceRequest {
    pub fn digest(&self) -> Result<String> {
        use sha2::{Digest, Sha256};
        self.validate()?;
        Ok(format!("{:x}", Sha256::digest(rudder_engine::protocol::canonical_json(self)?)))
    }
    pub fn from_json(data: &[u8]) -> Result<Self> {
        ensure!(data.len() <= 128 * 1024, "Operation request exceeds bound");
        let value: Value = serde_json::from_slice(data)?;
        for key in ["home_id", "communication_scope"] { if let Some(field) = value.get("scope").and_then(|scope| scope.get(key)) {text(field,256)?;} }
        let request: Self = serde_json::from_value(value)?; request.validate()?; Ok(request)
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(self.format_version == 1, "Unsupported operation version"); operation(&self.operation)?;
        ensure!(!self.idempotency_key.is_empty() && self.idempotency_key.len() <= 128 && self.idempotency_key.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"_.:-".contains(&byte)), "Invalid idempotency key");
        let s = &self.scope; text(&Value::String(s.project_id.clone()), 256)?;
        ensure!(s.owner_epoch > 0 && s.owner_epoch <= 9007199254740991 && s.agents.len() <= 64, "Invalid scoped epoch or selected agents");
        if s.location == Location::Cloud { text(&serde_json::to_value(&s.home_id)?, 256)?; }
        else { ensure!(s.home_id.is_none(), "Local execution cannot select a Cloud home"); }
        if let Some(scope) = &s.communication_scope { text(&Value::String(scope.clone()), 256)?; }
        let mut ids = std::collections::BTreeSet::new();
        for agent in &s.agents { text(&Value::String(agent.run_id.clone()), 256)?;
            ensure!(agent.run_epoch > 0 && agent.run_epoch <= 9007199254740991 && ids.insert(&agent.run_id), "Invalid selected agent epoch or duplicate"); }
        ensure!(s.selection != Selection::Agent || ids.len() == 1, "Agent action requires exactly one selected agent");
        ensure!(s.selection != Selection::Group || !ids.is_empty(), "Group action requires explicit selected agents");
        let a = &self.arguments;
        match self.operation.as_str() {
            "task.start.v1" => { fields(a, &["task", "intent"])?; text(&a["task"], 65536)?;
                ensure!(["automatic", "execute", "discuss", "plan_only"].contains(&a["intent"].as_str().unwrap_or("")) && s.selection == Selection::Project && ids.is_empty(), "Explicit task intent and project scope required"); }
            "agent.send.v1" | "agent.ask.v1" | "agent.reply.v1" => {
                let extra = if self.operation == "agent.ask.v1" { "request_id" } else if self.operation == "agent.reply.v1" { "reply_to" } else { "message" };
                fields(a, &["message", "ttl_ms", extra])?; text(&a["message"], 65536)?;
                if extra != "message" { text(&a[extra], 256)?; }
                if let Some(ttl) = a.get("ttl_ms") { ensure!(ttl.as_u64().is_some_and(|value| value > 0 && value <= 86400000), "Invalid message TTL"); }
                ensure!(matches!(s.selection, Selection::Agent | Selection::Group), "Messaging requires explicit selected agents"); }
            "agent.steer.v1" | "agent.interrupt.v1" | "agent.resume.v1" => {
                fields(a, &["reason", "instruction", "expected_thread_id", "expected_turn_id"])?;
                text(&a["reason"], 65536)?; text(&a["expected_thread_id"], 256)?; text(&a["expected_turn_id"], 256)?;
                if let Some(instruction) = a.get("instruction") { text(instruction, 65536)?; }
                ensure!(s.selection == Selection::Agent, "Live control requires one exact agent attempt"); }
            "agent.continue.v1" | "conversation.copy.v1" | "conversation.move.v1" => {
                fields(a, &["session_id", "next_step", "mode"])?; text(&a["session_id"], 256)?; text(&a["next_step"], 65536)?;
                let mode = if self.operation == "agent.continue.v1" {"continue"} else if self.operation == "conversation.copy.v1" {"copy"} else {"move"};
                ensure!(a["mode"] == mode && s.selection == Selection::Conversation, "Conversation mode must be explicit"); }
            "change.review.v1" | "change.check.v1" | "change.integrate.v1" => {
                fields(a, &["change_ids", "authorization"])?;
                let changes = a["change_ids"].as_array().ok_or_else(|| anyhow::anyhow!("Selected changes required"))?;
                ensure!(!changes.is_empty() && changes.len() <= 64, "Selected changes required"); for change in changes {text(change,256)?;}
                ensure!(a["authorization"].as_str() == self.operation.split('.').nth(1) && s.selection == Selection::Change, "Change authorization differs"); }
            "project.settings.v1" => { fields(a, &["setting", "value"])?; text(&a["value"], 256)?;
                ensure!(["model", "effort", "speed", "notifications", "sound", "color", "telemetry"].contains(&a["setting"].as_str().unwrap_or("")) && s.selection == Selection::Project, "Unknown project setting"); }
            name => {
                match name {
                    "surface.open.v1" | "agent.list.v1" | "agent.status.v1" | "agent.logs.v1" | "agent.watch.v1" | "agent.attach.v1" | "agent.report.v1" | "agent.find.v1" | "change.sync.v1" | "project.restart.v1" | "auth.login.v1" | "diagnostic.check.v1" | "diagnostic.setup.v1" => fields(a, &[])?,
                    "project.cloud.v1" | "project.board.v1" | "project.serve.v1" | "diagnostic.mouse.v1" | "diagnostic.feedback.v1" | "diagnostic.context.v1" | "diagnostic.improve.v1" => {
                        fields(a, &["argv"])?; let argv = a["argv"].as_array().ok_or_else(|| anyhow::anyhow!("Bounded adapter arguments required"))?;
                        ensure!(argv.len() <= 128, "Bounded adapter arguments required"); for arg in argv {text(arg,65536)?;}
                    }
                    "change.cleanup.v1" => { fields(a, &["force"])?; ensure!(a["force"].is_boolean(), "Explicit effect flag required"); }
                    "project.gc.v1" => { fields(a, &["dry_run"])?; ensure!(a["dry_run"].is_boolean(), "Explicit effect flag required"); }
                    "change.undo.v1" => { fields(a, &["op_id"])?; if let Some(id) = a.get("op_id") {text(id,256)?;} }
                    "agent.receipt.v1" => { fields(a, &["command_id"])?; text(&a["command_id"],256)?; }
                    "agent.topics.v1" => { fields(a, &["topics"])?; let topics = a["topics"].as_array().ok_or_else(|| anyhow::anyhow!("Bounded topics required"))?;
                        ensure!(topics.len() <= 256, "Bounded topics required"); for topic in topics {text(topic,256)?;} }
                    "agent.inbox.v1" => { fields(a, &["limit"])?; if let Some(limit) = a.get("limit") { ensure!(limit.as_u64().is_some_and(|value| value > 0 && value <= 64), "Invalid inbox limit"); } }
                    "agent.ack.v1" => { fields(a, &["message_id"])?; text(&a["message_id"], 256)?; }
                    "agent.await.v1" => { fields(a, &["timeout_ms", "command_id"])?; ensure!(a["timeout_ms"].as_u64().is_some_and(|value| value <= 30000), "Wait must be bounded to 30000 ms"); if let Some(id) = a.get("command_id") {text(id,256)?;} }
                    "agent.checkpoint.v1" => { fields(a, &["name"])?; text(&a["name"], 256)?; }
                    "agent.stop.v1" => { fields(a, &["reason"])?; text(&a["reason"], 65536)?; }
                    "agent.delete.v1" => { fields(a, &["force"])?; ensure!(a["force"].is_boolean(), "Explicit force flag required"); }
                    _ => bail!("Operation requires its existing adapter schema"),
                }
                ensure!(serde_json::to_value(&s.selection)?.as_str() == Some(operation(name)?.scope.as_str()), "Selection differs from operation scope");
            }
        }
        if matches!(self.operation.as_str(), "agent.send.v1" | "agent.ask.v1" | "agent.steer.v1" | "agent.interrupt.v1" | "agent.resume.v1") { text(&serde_json::to_value(&s.communication_scope)?, 256)?; }
        if self.operation == "agent.reply.v1" { ensure!(s.selection == Selection::Agent, "Reply targets its original conversation"); }
        ensure!(serde_json::to_vec(self)?.len() <= 128 * 1024, "Operation request exceeds bound"); Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SurfaceReceipt {
    pub format_version: u32, pub operation: String, pub idempotency_key: String,
    pub request_sha256: String, pub kind: String, pub state: String, pub result: Value,
}
impl SurfaceReceipt {
    pub fn validate(&self, original: &SurfaceRequest) -> Result<()> {
        ensure!(self.format_version == 1 && self.operation == original.operation && self.idempotency_key == original.idempotency_key
            && self.request_sha256 == original.digest()? && self.kind == operation(&original.operation)?.receipt, "Receipt differs from original operation and scope");
        ensure!(["accepted", "running", "completed", "rejected", "effect_unknown"].contains(&self.state.as_str()), "Invalid operation receipt"); Ok(())
    }
}

#[cfg(test)]
#[path="tests.rs"]
mod tests;
