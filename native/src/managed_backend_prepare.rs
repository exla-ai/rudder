//! Pure managed model-backend preparation, independent of workspace ownership.
//! Call only for an explicitly selected profile and already reserved fence.
//! No profile lookup, inherited environment, IO, model call, scheduler or readiness.
use anyhow::{ensure, Context, Result};
use rudder_engine::{execution::{HomeFile, ManagedFrontendStart, ReadOnlyRuntime},
    protocol::canonical_json};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::{Path, PathBuf}};
use super::ManagedLaunchFence;

/// No Debug/Serialize implementation: values belong only in a sealed spec.
pub(crate) struct SelectedCredentials {
    pub(super) profile_id: String,
    backend: crate::Backend,
    pub(super) environment: std::collections::BTreeMap<String, String>,
}

/// One selected host-owned key. Never Debug/Serde, guest env, home JSON or CAS.
pub(crate) struct ManagedCodexApiKey(String);
impl ManagedCodexApiKey {
    pub(crate) fn expose_for_login(&self) -> &str { &self.0 }
}
impl Drop for ManagedCodexApiKey {
    fn drop(&mut self) { unsafe { self.0.as_bytes_mut().fill(0); } }
}
impl SelectedCredentials {
    pub(crate) fn check_backend(&self, backend: &ManagedBackendProfile) -> Result<()> {
        ensure!(backend.backend() == self.backend,
            "selected credential profile belongs to another backend");
        Ok(())
    }
    /// Copy only this selected API-key environment into the private sealed spec.
    /// Codex's key must already have moved into its zeroizing host-only owner.
    pub(crate) fn guest_environment(&self, fence: &ManagedLaunchFence,
        reports: &ManagedReportMapping) -> Result<BTreeMap<String, String>> {
        ensure!(self.backend != crate::Backend::Codex || self.environment.is_empty(),
            "selected Codex API key must move to host custody before guest preparation");
        let mut environment = self.environment.clone();
        environment.extend([("RUDDER_FABRIC_ATTEMPT_ID".into(), fence.attempt_id.clone()),
            ("RUDDER_DONE_FILE".into(), reports.guest_done_file.display().to_string()),
            ("CODEX_HOME".into(), "/home/agent/.codex".into())]);
        Ok(environment)
    }
    /// The selected profile loader supplies exactly one key, never its HOME or
    /// inherited environment. API keys have no refresh/writeback semantics.
    pub(crate) fn api_key(profile_id: String, backend: crate::Backend, secret: String) -> Result<Self> {
        ensure!(!profile_id.is_empty() && profile_id.len() <= 256 && !profile_id.chars().any(char::is_control), "invalid selected credential profile");
        ensure!(!secret.is_empty() && secret.len() <= 16 * 1024 && !secret.contains(['\0', '\r', '\n']), "invalid selected provider credential");
        let key = match backend {
            crate::Backend::Codex => "OPENAI_API_KEY",
            crate::Backend::Claude => "ANTHROPIC_API_KEY",
            _ => anyhow::bail!("managed provider profile is unsupported"),
        };
        Ok(Self { profile_id, backend, environment: [(key.into(), secret)].into() })
    }
    /// Ordinary CLI profiles do not participate in a shared refresh lease.
    /// Refuse before reading/copying a refresh token or invalidating a live login.
    pub(crate) fn oauth_profile(_profile_id: &str) -> Result<Self> {
        Err(std::io::Error::new(std::io::ErrorKind::Unsupported,
            "managed OAuth requires a qualified exclusive refresh and protected writeback adapter").into())
    }
    pub(crate) fn take_host_codex_key(&mut self) -> Result<Option<ManagedCodexApiKey>> {
        if self.backend != crate::Backend::Codex { return Ok(None); }
        ensure!(self.environment.len() == 1, "managed Codex requires exactly its selected API key");
        Ok(Some(ManagedCodexApiKey(self.environment.remove("OPENAI_API_KEY")
            .context("selected Codex profile has no API key")?)))
    }
}
impl Drop for SelectedCredentials {
    fn drop(&mut self) {
        for value in self.environment.values_mut() { unsafe { value.as_bytes_mut().fill(0); } }
    }
}

/// Explicit backend process layout. Both commands contain guest paths only.
/// The helper launches the server directly and reports its held kernel identity.
pub(crate) enum ManagedBackendProfile {
    CodexAppServer { program: String, arguments: Vec<String> },
    ClaudeSdk { node: String, rudder_cli: String, claude: String,
        arguments: Vec<String>, initial_prompt: Option<String> },
}
impl ManagedBackendProfile {
    /// A launch layout is not readiness or authority to send this packet.
    pub(crate) fn frontend_start(&self) -> ManagedFrontendStart {
        match self { Self::CodexAppServer { .. } => ManagedFrontendStart::AfterHostApiKeyV1,
            Self::ClaudeSdk { .. } => ManagedFrontendStart::Immediate }
    }
    /// Preserve the caller's companion order, then validate this explicit profile.
    /// No command inspection can promote an offline command into this enum.
    pub(crate) fn validate_runtime(&self, runtime: &[ReadOnlyRuntime],
        companion_programs: &[&str]) -> Result<()> {
        for program in companion_programs { validate_runtime_program(program, runtime)?; }
        match self {
            Self::CodexAppServer { program, .. } => validate_runtime_program(program, runtime),
            Self::ClaudeSdk { node, rudder_cli, claude, .. } => {
                for program in [node, rudder_cli, claude] { validate_runtime_program(program, runtime)?; }
                Ok(())
            }
        }
    }
    pub(crate) fn backend(&self) -> crate::Backend {
        match self { Self::CodexAppServer { .. } => crate::Backend::Codex,
            Self::ClaudeSdk { .. } => crate::Backend::Claude }
    }
    pub(crate) fn execution_profile(&self) -> &'static str {
        match self { Self::CodexAppServer { .. } => "codex-app-server",
            Self::ClaudeSdk { .. } => "claude-sdk" }
    }
}

/// Guest-owned candidate reports. These paths convey no backend/process,
/// acknowledgment, writer-barrier or checkpoint authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ManagedReportMapping {
    pub host_done_file: PathBuf,
    pub guest_done_file: PathBuf,
    pub host_signal_file: PathBuf,
    pub guest_signal_file: PathBuf,
}

impl ManagedReportMapping {
    pub(crate) fn for_execution(home: &Path) -> Self {
        Self { host_done_file: home.join(".rudder-reports/done.json"),
            guest_done_file: "/home/agent/.rudder-reports/done.json".into(),
            host_signal_file: home.join(".rudder-reports/signal.json"),
            guest_signal_file: "/home/agent/.rudder-reports/signal.json".into() }
    }
}

/// Private launch material. No Debug/Serde: home files may include a prompt.
/// No process, ACK, source grant or readiness witness is created here.
pub(crate) struct ManagedBackendLayout {
    pub command: Vec<String>,
    pub server_command: Option<Vec<String>>,
    pub home_files: Vec<HomeFile>,
    pub reports: ManagedReportMapping,
    pub execution_profile: &'static str,
    pub frontend_start: ManagedFrontendStart,
}

/// Pure preparation for both the existing local owner and a future qualified
/// source-model owner. The latter must supply its own assignment/authority,
/// runtime validation, sealed-spec family and live auth/readiness integration.
pub(crate) fn prepare_layout(fence: &ManagedLaunchFence, backend: &ManagedBackendProfile,
    native_guest: &str, socket: &Path, execution_home: &Path) -> Result<ManagedBackendLayout> {
    let (command, server_command, home_files) = prepare_backend(fence, backend, native_guest, socket)?;
    Ok(ManagedBackendLayout { command, server_command, home_files,
        reports: ManagedReportMapping::for_execution(execution_home),
        execution_profile: backend.execution_profile(), frontend_start: backend.frontend_start() })
}

pub(crate) fn selected_protected_roots(identity_home: &Path, additional: &[PathBuf]) -> Result<Vec<PathBuf>> {
    ensure!(additional.len() <= 32, "managed protected credential roots exceed bound");
    let roots: Vec<_> = std::iter::once(identity_home.to_path_buf()).chain(additional.iter().cloned()).collect();
    for root in &roots {
        ensure!(root.is_absolute() && root != Path::new("/") && root.components().all(|component|matches!(component,
            std::path::Component::RootDir | std::path::Component::Normal(_))), "managed protected roots must be explicit normalized host paths");
    }
    Ok(roots)
}

pub(crate) fn validate_runtime_program(program: &str, runtime: &[ReadOnlyRuntime]) -> Result<()> {
    let path = Path::new(program);
    ensure!(path.is_absolute() && path.components().all(|component|matches!(component,
        std::path::Component::RootDir | std::path::Component::Normal(_)))
        && runtime.iter().any(|projection|path.starts_with(&projection.guest_path)),
        "managed programs must belong to the declared read-only image runtime");
    Ok(())
}

pub(super) fn prepare_backend(fence: &ManagedLaunchFence, backend: &ManagedBackendProfile, native_guest: &str,
    socket: &Path) -> Result<(Vec<String>, Option<Vec<String>>, Vec<HomeFile>)> {
    let broker_args = vec!["fabric", "mcp", "--identity-file", "/run/rudder/agent/identity.json", "--broker-socket", "/run/rudder/agent/broker.sock"];
    let config = canonical_json(&serde_json::json!({"mcpServers":{"rudder_agents":{"type":"stdio",
        "command":native_guest,"args":broker_args}}}))?;
    let signal = Path::new("/home/agent/.rudder-reports/signal.json");
    let mut files = vec![HomeFile { relative_path: ".rudder/mcp.json".into(), contents: config },
        HomeFile { relative_path: ".rudder-reports/assignment.json".into(), contents: canonical_json(fence)? }];
    let mut codex_configs = vec!["-c".into(), format!("mcp_servers.rudder_agents.command={}", serde_json::to_string(native_guest)?),
        "-c".into(), format!("mcp_servers.rudder_agents.args={}", serde_json::to_string(&broker_args)?),
        "-c".into(), "mcp_servers.rudder_agents.enabled=true".into(),
        "-c".into(), "notify=[\"/bin/sh\",\"/home/agent/.rudder/notify.sh\"]".into(),
        "-c".into(), "cli_auth_credentials_store=\"ephemeral\"".into()];
    match backend {
        ManagedBackendProfile::CodexAppServer { program, arguments } => {
            ensure!(Path::new(program).is_absolute() && !arguments.iter().any(|a| matches!(a.as_str(), "exec" | "app-server" | "--remote") || a.starts_with("--remote=")), "invalid explicit Codex interactive profile");
            files.push(HomeFile { relative_path: ".rudder/notify.sh".into(), contents: crate::signals::codex_notify_script(signal).into_bytes() });
            files.push(HomeFile { relative_path:".codex/config.toml".into(), contents:b"cli_auth_credentials_store = \"ephemeral\"\n".to_vec() });
            let mut server = vec![program.clone()];
            // The server owns MCP and lifecycle configuration. Preserve explicit
            // root options before applying this attempt's guest-only overrides.
            let mut at = 0;
            while at < arguments.len() {
                let arg = &arguments[at];
                if arg == "--" { break; }
                if matches!(arg.as_str(), "-c" | "--config" | "--enable" | "--disable" | "--profile" | "-p") {
                    let value = arguments.get(at + 1).context("Codex root option is missing its value")?;
                    server.extend([arg.clone(), value.clone()]); at += 2;
                } else if ["--config=", "--enable=", "--disable=", "--profile="].iter().any(|prefix| arg.starts_with(prefix)) {
                    server.push(arg.clone()); at += 1;
                } else { at += 1; }
            }
            server.append(&mut codex_configs);
            server.extend(["app-server".into(), "--listen".into(), format!("unix://{}", socket.display())]);
            let mut terminal = vec![program.clone(), "--remote".into(), format!("unix://{}", socket.display())];
            terminal.extend(arguments.clone());
            let before_prompt = terminal.iter().position(|arg|arg == "--").unwrap_or(terminal.len());
            terminal.splice(before_prompt..before_prompt, ["-c".into(), "cli_auth_credentials_store=\"ephemeral\"".into()]);
            Ok((terminal, Some(server), files))
        }
        ManagedBackendProfile::ClaudeSdk { node, rudder_cli, claude, arguments, initial_prompt } => {
            for path in [node, rudder_cli, claude] { ensure!(Path::new(path).is_absolute(), "managed Claude programs must use explicit guest paths"); }
            files.push(HomeFile { relative_path: ".rudder/claude-settings.json".into(), contents: crate::signals::claude_settings_json(signal, false).into_bytes() });
            let mut arguments = arguments.clone(); arguments.extend(["--mcp-config".into(), "/home/agent/.rudder/mcp.json".into(),
                "--settings".into(), "/home/agent/.rudder/claude-settings.json".into()]);
            files.push(HomeFile { relative_path: ".rudder/claude-launch.json".into(), contents: canonical_json(&serde_json::json!({
                "format_version":1,"run_id":fence.run_id,"attempt_id":fence.attempt_id,"socket_path":socket,
                "claude_program":claude,"claude_args":arguments,"initial_prompt":initial_prompt}))? });
            Ok((vec![node.clone(), rudder_cli.clone(), "__claude-sdk".into(), "--launch-file".into(), "/home/agent/.rudder/claude-launch.json".into()], None, files))
        }
    }
}

#[cfg(test)]
#[path = "managed_backend_prepare_tests.rs"]
mod tests;
