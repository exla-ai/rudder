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

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionProfileV4 {
    pub format_version: u32,
    pub request_id: String,
    pub network: NetworkProfileV1,
    pub workspace: ExecutionWorkspaceSelectionV4,
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
            ensure!(
                matches!(
                    self.workspace,
                    ExecutionWorkspaceSelectionV4::RemoteSourceOnly { .. }
                ),
                "execution specification5 requires remote source-only workspace"
            );
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
