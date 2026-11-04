//! Sealed v4 selection data. Matching this profile never creates a live grant,
//! kernel witness, accepted-history proof or permission to execute.
use super::{
    network::{ExpectedNetworkBinding, NetworkMode, NetworkProfileV1, ValidatedNetworkProfile},
    ExecutionSpec, ProtectedHostContextRef,
};
use crate::{protocol::canonical_json, vfs::remote_launch::RemoteLaunchRequest};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::Digest;
use uuid::Uuid;

/// Non-secret selection recorded alongside the existing execution identity.
/// This describes selection, not a credential grant or model qualification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelProfileSelectionV1 {
    pub format_version: u32,
    pub backend: String,
    pub profile_id: String,
    pub provider: String,
    pub credential_kind: String,
    pub requested_model: String,
    pub model: String,
    pub requested_effort: Option<String>,
    pub effort: Option<String>,
    pub backend_version: String,
}

#[cfg(test)]
mod model_identity_tests {
    use super::*;
    fn selection() -> ModelProfileSelectionV1 {
        ModelProfileSelectionV1 { format_version: 1, backend: "codex".into(), profile_id: "private-selected".into(),
            provider: "openai-codex".into(), credential_kind: "oauth".into(), requested_model: "account-model".into(),
            model: "account-model".into(), requested_effort: Some("max".into()), effort: Some("xhigh".into()),
            backend_version: "installed-version".into() }
    }
    fn execution() -> super::super::ExecutionIdentityV2 {
        super::super::ExecutionIdentityV2 { project_id: "project".into(), request_id: Uuid::new_v4().to_string(),
            run_id: "run".into(), run_epoch: 1, execution_id: Uuid::new_v4().to_string(), operation_id: Uuid::new_v4().to_string(),
            view_id: Uuid::new_v4().to_string(), owner: crate::protocol::OwnerEpoch { owner: "owner".into(), epoch: 1 },
            request_digest: "a".repeat(64), supervisor_pid: 100 }
    }
    #[test]
    fn accepted_history_model_receipt_survives_restart_and_rejects_resume_drift() {
        let selected = selection(); let execution = execution();
        let receipt = ExecutionProfileV4::bind_model_session(&serde_json::to_value(&selected).unwrap(), &execution,
            "actual-session", "account-model", Some("xhigh")).unwrap();
        let restored: ModelSessionReceiptV1 = serde_json::from_slice(&canonical_json(&receipt).unwrap()).unwrap();
        restored.validate_resume(&selected, &execution, "actual-session").unwrap();
        let mut changed = selected.clone(); changed.profile_id = "other-profile".into();
        assert!(restored.validate_resume(&changed, &execution, "actual-session").is_err());
        changed = selected.clone(); changed.requested_effort = Some("xhigh".into());
        assert!(restored.validate_resume(&changed, &execution, "actual-session").is_err());
        let mut moved = execution.clone(); moved.owner.epoch += 1;
        assert!(restored.validate_resume(&selected, &moved, "actual-session").is_err());
        assert!(restored.validate_resume(&selected, &execution, "other-session").is_err());
    }
    #[test]
    fn accepted_history_model_receipt_rejects_backend_model_and_version_changes() {
        let selected = serde_json::to_value(selection()).unwrap(); let execution = execution();
        assert!(ExecutionProfileV4::bind_model_session(&selected, &execution, "session", "fallback-model", None).is_err());
        assert!(ExecutionProfileV4::bind_model_session(&selected, &execution, "session", "account-model", Some("high")).is_err());
        let mut changed = selected.clone(); changed["format_version"] = serde_json::json!(2);
        assert!(ExecutionProfileV4::validate_selected_model(&changed).is_err());
        changed = selected.clone(); changed["secret"] = serde_json::json!("must-not-be-in-a-receipt");
        assert!(ExecutionProfileV4::validate_selected_model(&changed).is_err());
        let receipt = ExecutionProfileV4::bind_model_session(&selected, &execution, "session", "account-model", None).unwrap();
        changed = selected.clone(); changed["backend_version"] = serde_json::json!("different-installed-version");
        assert!(ExecutionProfileV4::validate_model_resume(&receipt, &changed, &execution, "session").is_err());
        assert!(receipt["observed_effort"].is_null());
    }
}
impl ModelProfileSelectionV1 {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.format_version == 1, "unsupported model profile version");
        for value in [&self.profile_id, &self.requested_model, &self.model, &self.backend_version] {
            model_identity_text(value)?;
        }
        ensure!(matches!(self.credential_kind.as_str(), "api_key" | "oauth" | "token"), "invalid model credential kind");
        ensure!(match self.backend.as_str() {
            "claude" => self.provider == "anthropic",
            "codex" => self.provider == "openai" || self.provider == "openai-codex" && self.credential_kind == "oauth",
            _ => false,
        }, "selected model profile provider/backend differs");
        for value in [self.requested_effort.as_deref(), self.effort.as_deref()].into_iter().flatten() {
            ensure!(matches!(value, "low" | "medium" | "high" | "xhigh" | "max"), "invalid selected model effort");
        }
        let effective = match (self.backend.as_str(), self.requested_effort.as_deref()) {
            ("codex", Some("max")) => Some("xhigh"),
            ("codex", None) => Some("high"),
            (_, value) => value,
        };
        ensure!(self.effort.as_deref() == effective, "selected model effort differs from launch policy");
        Ok(())
    }
}

/// Written by the trusted owner only after actual backend initialization. JSON
/// can describe this fact but cannot restore a process, authorize a resume or
/// establish completion, verification, acceptance, push or deployment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelSessionReceiptV1 {
    pub format_version: u32,
    pub selection: ModelProfileSelectionV1,
    pub execution: super::ExecutionIdentityV2,
    pub session_id: String,
    pub observed_model: String,
    /// None means the backend did not report effort, never that it verified it.
    pub observed_effort: Option<String>,
}
impl ModelSessionReceiptV1 {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.format_version == 1, "unsupported model session receipt version");
        self.selection.validate()?;
        self.execution.validate()?;
        model_identity_text(&self.session_id)?;
        model_identity_text(&self.observed_model)?;
        ensure!(self.observed_model == self.selection.model, "backend model differs from selected model");
        if let Some(effort) = &self.observed_effort {
            ensure!(Some(effort) == self.selection.effort.as_ref(), "backend effort differs from selected effort");
        }
        Ok(())
    }
    pub fn validate_resume(&self, selection: &ModelProfileSelectionV1,
        execution: &super::ExecutionIdentityV2, session_id: &str) -> Result<()> {
        self.validate()?;
        selection.validate()?;
        execution.validate()?;
        ensure!(&self.selection == selection && &self.execution == execution && self.session_id == session_id,
            "resume profile/model/effort/session or execution owner changed");
        Ok(())
    }
}
fn model_identity_text(value: &str) -> Result<()> {
    ensure!(!value.is_empty() && value.len() <= 256 && value.trim() == value
        && !value.chars().any(char::is_control), "invalid model session identity");
    Ok(())
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionProfileV4 {
    pub format_version: u32,
    pub request_id: String,
    pub network: NetworkProfileV1,
    pub workspace: ExecutionWorkspaceSelectionV4,
    /// Spec5's original startup constraint. Omission preserves Spec4 bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup: Option<super::startup_budget::StartupConstraint>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionWorkspaceSelectionV4 {
    LocalComplete {},
    RemoteSourceOnly {
        selection: RemoteLaunchRequest,
        /// Credentials remain in this protected host-only object, outside CAS
        /// and guest projections. This reference contains no credential bytes.
        host_context: ProtectedHostContextRef,
    },
}

impl ExecutionProfileV4 {
    /// Strict codecs usable by native callers without extending legacy Spec4/5
    /// encodings. Persist this sidecar under the original execution's custody.
    pub fn validate_selected_model(value: &serde_json::Value) -> Result<()> {
        let selected: ModelProfileSelectionV1 = serde_json::from_value(value.clone())?;
        selected.validate()
    }
    pub fn bind_model_session(selection: &serde_json::Value, execution: &super::ExecutionIdentityV2,
        session_id: &str, model: &str, effort: Option<&str>) -> Result<serde_json::Value> {
        let receipt = ModelSessionReceiptV1 { format_version: 1,
            selection: serde_json::from_value(selection.clone())?, execution: execution.clone(),
            session_id: session_id.into(), observed_model: model.into(), observed_effort: effort.map(str::to_owned) };
        receipt.validate()?;
        Ok(serde_json::to_value(receipt)?)
    }
    pub fn validate_model_resume(receipt: &serde_json::Value, selection: &serde_json::Value,
        execution: &super::ExecutionIdentityV2, session_id: &str) -> Result<()> {
        let receipt: ModelSessionReceiptV1 = serde_json::from_value(receipt.clone())?;
        let selection: ModelProfileSelectionV1 = serde_json::from_value(selection.clone())?;
        receipt.validate_resume(&selection, execution, session_id)
    }
    /// Derives protocol data only. Endpoint ownership and authorization still
    /// come from the actual remote view and trusted helper services.
    pub fn partial_binding(&self) -> Result<Option<crate::vfs::partial_rpc::PartialBinding>> {
        let ExecutionWorkspaceSelectionV4::RemoteSourceOnly { selection, .. } = &self.workspace
        else {
            return Ok(None);
        };
        selection.validate()?;
        let binding = crate::vfs::partial_rpc::PartialBinding {
            protocol: crate::vfs::partial_rpc::PROTOCOL.into(),
            format_version: 1,
            launch_request_digest: hex::encode(sha2::Sha256::digest(canonical_json(selection)?)),
            root: selection.root.clone(),
            attempt: selection.attempt.clone(),
            observation: selection.observation.clone(),
        };
        binding.validate()?;
        Ok(Some(binding))
    }
    pub(crate) fn validate_metadata(&self, spec: &ExecutionSpec) -> Result<()> {
        ensure!(self.format_version == 1, "unsupported v4 execution profile");
        ensure!(
            Uuid::parse_str(&self.request_id)?.to_string() == self.request_id,
            "execution request identifier must be canonical UUID"
        );
        let broker = spec
            .broker_binding
            .as_ref()
            .context("v4 profile requires a scoped broker")?;
        let observation = spec
            .workspace_observation
            .as_ref()
            .context("v4 profile requires workspace authority")?;
        self.validated_network(spec)?;
        // Spec5 is a separate remote-only metadata family. Admission still
        // requires owned source/controller lifetimes outside this codec.
        if spec.format_version == 5 {
            self.startup.as_ref().context("source specification requires its original startup constraint")?
                .validate()?;
            ensure!(
                matches!(
                    self.workspace,
                    ExecutionWorkspaceSelectionV4::RemoteSourceOnly { .. }
                ),
                "execution specification5 requires remote source-only workspace"
            );
        } else {
            ensure!(self.startup.is_none(), "legacy specification cannot carry a source startup constraint");
        }
        if let ExecutionWorkspaceSelectionV4::RemoteSourceOnly {
            selection,
            host_context,
        } = &self.workspace
        {
            selection.validate()?;
            host_context.validate()?;
            ensure!(
                selection.request_id == self.request_id
                    && selection.observation == observation.binding
                    && selection.attempt.project_id == broker.project_id
                    && selection.attempt.run_id == broker.run_id
                    && selection.attempt.run_epoch == broker.run_epoch
                    && selection.attempt.owner == spec.owner
                    && selection.attempt.execution_id == spec.execution_id
                    && selection.attempt.operation_id == spec.operation_id
                    && selection.attempt.view_id == spec.view_id,
                "remote selection differs from the sealed execution profile"
            );
            ensure!(
                observation.application.is_none(),
                "remote VCS application is not qualified"
            );
            let context = host_context
                .path
                .canonicalize()
                .context("remote host context is unavailable")?;
            ensure!(
                spec.authority_roots
                    .iter()
                    .map(|root| root.canonicalize())
                    .collect::<std::io::Result<Vec<_>>>()?
                    .iter()
                    .any(|root| context.starts_with(root)),
                "remote host context requires a declared authority root"
            );
        }
        ensure!(
            canonical_json(self)?.len() <= 40 * 1024,
            "v4 execution profile exceeds40KiB"
        );
        Ok(())
    }
    pub(crate) fn validated_network(
        &self,
        spec: &ExecutionSpec,
    ) -> Result<ValidatedNetworkProfile> {
        let broker = spec
            .broker_binding
            .as_ref()
            .context("v4 network requires a scoped broker")?;
        let network = &self.network.binding;
        let expected = ExpectedNetworkBinding {
            project_id: &broker.project_id,
            owner: &spec.owner.owner,
            owner_epoch: spec.owner.epoch,
            run_id: &broker.run_id,
            run_epoch: broker.run_epoch,
            request_id: &self.request_id,
            execution_id: &spec.execution_id,
            operation_id: &spec.operation_id,
            view_id: &spec.view_id,
            network_id: &network.network_id,
            readiness_nonce: &self.network.readiness_nonce,
        };
        // The first physical composition is offline. Connected qualification
        // must gain its own actual proxy/readiness/drain before admission.
        ensure!(
            self.network.mode == NetworkMode::OfflineV1,
            "connected v4 network profile is not qualified"
        );
        Ok(NetworkProfileV1::from_json(
            &canonical_json(&self.network)?,
            &expected,
            None,
        )?)
    }
}
