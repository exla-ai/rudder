//! Bounded source-only remote selection for a trusted pre-mount factory.
//! Expected accepted source/seed identities are sealed inputs, never evidence
//! of current project/fleet acceptance or private jj/history validation. The
//! host must establish those facts independently before VCS/backend authority.
//! These types establish neither launch permission nor checkpoint authority.

use super::{remote::Resolver, rpc::ObservationBinding, RemoteOptions, VfsManager, VfsView};
use crate::{
    protocol::{canonical_json, ObjectKind, ObjectRef},
    remote_root::{AttemptBinding, RemoteSession, RootBinding},
    store::{ImportPolicy, LocalStore},
    workspace::WorkspaceProfile,
};
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use std::{path::Path, sync::Arc};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteLaunchRequest {
    pub format_version: u32,
    pub request_id: String,
    pub root: RootBinding,
    pub attempt: AttemptBinding,
    pub observation: ObservationBinding,
    /// Exact expected VCS projection-policy blob, distinct from the profile's
    /// subtree-exclusion digest in observation. No policy is guessed here.
    pub projection_policy: ObjectRef,
    /// Expected identities only. The accepted seed has no edge in RunnableRoot
    /// and is deliberately not fetched, imported or pinned by this validator.
    pub accepted_source: ObjectRef,
    pub accepted_seed: ObjectRef,
}
impl RemoteLaunchRequest {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.format_version == 1,
            "unsupported remote launch selection"
        );
        self.root
            .scope
            .digest()
            .map_err(|error| super::error(error.errno()))?;
        for value in [
            &self.request_id,
            &self.attempt.execution_id,
            &self.attempt.operation_id,
            &self.attempt.view_id,
        ] {
            ensure!(
                Uuid::parse_str(value)?.to_string() == *value,
                "remote launch identifier must be canonical UUID"
            );
        }
        for value in [
            &self.root.scope.provider_id,
            &self.root.scope.tenant_id,
            &self.root.root_id,
            &self.attempt.project_id,
            &self.attempt.run_id,
            &self.attempt.owner.owner,
        ] {
            ensure!(
                !value.is_empty() && value.len() <= 512 && !value.chars().any(char::is_control),
                "invalid remote launch identity"
            );
        }
        self.attempt.owner.validate()?;
        ensure!(
            self.attempt.run_epoch > 0,
            "remote launch run epoch must be positive"
        );
        self.observation.validate()?;
        ensure!(
            self.observation.attempt.execution_id == self.attempt.execution_id
                && self.observation.attempt.operation_id == self.attempt.operation_id
                && self.observation.view_id == self.attempt.view_id
                && self.observation.owner == self.attempt.owner,
            "remote launch observation differs from attempt"
        );
        for (reference, kind) in [
            (&self.root.root, ObjectKind::RunnableRoot),
            (&self.accepted_source, ObjectKind::SourceRoot),
            (&self.accepted_seed, ObjectKind::Directory),
            (&self.projection_policy, ObjectKind::Blob),
        ] {
            reference.validate()?;
            ensure!(
                reference.kind == kind,
                "incorrect remote launch reference kind"
            );
        }
        ensure!(
            self.observation.exclusion_policy_digest
                == super::exclusion_policy_digest(&ImportPolicy::default())?,
            "remote launch requires the source-only exclusion policy"
        );
        ensure!(
            canonical_json(self)?.len() <= 16 * 1024,
            "remote launch selection exceeds 16 KiB"
        );
        Ok(())
    }
}

/// Opaque, non-deserializable selection backed by the actual admitted session.
/// It validates metadata only and cannot authorize a launch or remote checkpoint.
pub struct ValidatedRemoteSelection {
    pub(super) request: RemoteLaunchRequest,
    pub(super) resolver: Arc<Resolver>,
}
impl ValidatedRemoteSelection {
    pub fn request(&self) -> &RemoteLaunchRequest {
        &self.request
    }
    pub fn profile(&self) -> &WorkspaceProfile {
        self.resolver.profile()
    }
    pub fn check_current(&self) -> Result<()> {
        self.resolver.guard()
    }

    /// Off-actor private namespace setup only. The returned owner retains the
    /// actual source/cache/registry through finalization. A trusted factory must
    /// independently validate accepted seed/head, exact spec and current attempt
    /// authority, retaining any additional external resources itself.
    pub fn open(
        self,
        manager_root: impl AsRef<Path>,
        delta_store: LocalStore,
    ) -> Result<RemoteLaunchWorkspace> {
        self.open_inner(manager_root, delta_store, false)
    }
    /// Explicit namespace5 lifecycle. Initializes capture/freeze journals before
    /// exposure, but grants no process quiescence or execution authority.
    pub fn open_finalizable(
        self,
        manager_root: impl AsRef<Path>,
        delta_store: LocalStore,
    ) -> Result<RemoteLaunchWorkspace> {
        self.open_inner(manager_root, delta_store, true)
    }
    fn open_inner(
        self,
        manager_root: impl AsRef<Path>,
        delta_store: LocalStore,
        finalizable: bool,
    ) -> Result<RemoteLaunchWorkspace> {
        self.check_current()?;
        let manager = VfsManager::open_remote_selection(manager_root, delta_store, self.resolver)?;
        let view = if finalizable {
            manager.fork_remote_finalizable(&self.request.observation.namespace)?
        } else {
            manager.fork_remote_with_namespace(&self.request.observation.namespace)?
        };
        self::check_workspace(&manager, &view, &self.request)?;
        Ok(RemoteLaunchWorkspace {
            manager,
            view,
            request: self.request,
        })
    }
}

/// An owned exact remote namespace, never a complete pin or quiescence receipt.
/// Both manager and view retain the Resolver, actual RemoteSession, registry,
/// cache, authority and their source objects. NodeRemoteSource keeps pending
/// transport admission until its terminal response or owned-child reap; cache
/// workers retain the source and running charge until fetch actually returns.
/// External resources not encapsulated by that source, such as a future
/// TransferSession, need the outer factory's separate owned resource guard.
pub struct RemoteLaunchWorkspace {
    manager: VfsManager,
    view: VfsView,
    request: RemoteLaunchRequest,
}
impl RemoteLaunchWorkspace {
    pub fn request(&self) -> &RemoteLaunchRequest {
        &self.request
    }
    pub fn manager(&self) -> &VfsManager {
        &self.manager
    }
    pub fn view(&self) -> &VfsView {
        &self.view
    }
    /// Moves the actual live resource chain without reconstructing it from the
    /// returned request. Keeping either manager or view retains that chain.
    /// Dropping both is cancellation/cleanup, never proof of transport drain.
    pub fn into_parts(self) -> (VfsManager, VfsView, RemoteLaunchRequest) {
        (self.manager, self.view, self.request)
    }
}

/// Blocking bounded metadata reads outside the App/engine actor. Saved request
/// bytes cannot mint a RemoteSession; admission must freshly validate its grant.
pub fn validate_selection(
    session: RemoteSession,
    expected: &RemoteLaunchRequest,
    options: RemoteOptions,
) -> Result<ValidatedRemoteSelection> {
    expected.validate()?;
    session
        .check_current()
        .map_err(|error| super::error(error.errno()))?;
    ensure!(
        session.binding() == &expected.root && session.attempt() == &expected.attempt,
        "remote launch selection differs from the actual admitted session"
    );
    let resolver = Arc::new(Resolver::for_launch(session, expected, options)?);
    resolver.guard()?;
    Ok(ValidatedRemoteSelection {
        request: expected.clone(),
        resolver,
    })
}

fn check_workspace(
    manager: &VfsManager,
    view: &VfsView,
    request: &RemoteLaunchRequest,
) -> Result<()> {
    ensure!(
        manager.remote_root_binding() == Some(&request.root)
            && view.id() == request.observation.view_id,
        "remote launch workspace differs from selected root/view"
    );
    let cursor = view.dirty_cursor()?;
    ensure!(
        cursor.owner == request.observation.owner
            && cursor.namespace == request.observation.namespace,
        "remote launch workspace differs from reserved observation namespace"
    );
    Ok(())
}
