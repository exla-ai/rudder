//! Host-only source authorization, separate from guest tools/partial ingress.
//! These helpers allocate no owner, endpoint, token, accepted history or live
//! authority. The future trusted Native issuer must hold actual EngineOwnership
//! and mint a separate host-only capability; saved fences/scopes are expectations.
use super::*;
use crate::{protocol::canonical_json, vfs::partial_rpc::PartialBinding};

pub const HOST_SOURCE_PURPOSE: &str = "remote_source_only_v1";
pub const HOST_SOURCE_REQUIRED_OPERATION: &str = "source:host:authorize";
const SCOPE_DOMAIN: &[u8] = b"rudder_host_source_authorization_v1\0";

/// Read-only data from one capability/current-attempt transaction. This snapshot
/// does not hold authority or establish a freshness window. A host adapter must
/// sample trusted time again after the query and conservatively bound remaining
/// capability lifetime by its original suspend-aware request deadline. Clock or
/// conversion uncertainty must refuse authorization; a late result cannot renew
/// the request. `None` removes only the token-expiry bound, never that deadline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostSourceAuthorization {
    pub identity: CapabilityIdentity,
    /// The trusted host `now_ms` supplied for this transaction's validation.
    pub validated_at_ms: i64,
    /// Expiry of the exact authenticated grant read in the same transaction.
    pub expires_at_ms: Option<i64>,
}

/// Domain-separated expectation over the exact complete canonical PartialBinding
/// (launch digest, provider/tenant/root, project/run/owner, execution, operation,
/// view, namespace, profile and exclusion policy). This string grants nothing.
/// It is deliberately neither partial_scope nor complete workspace_scope.
pub fn host_source_scope(binding: &PartialBinding) -> Result<String> {
    binding.validate()?;
    let mut hash = Sha256::new();
    hash.update(SCOPE_DOMAIN);
    hash.update(HOST_SOURCE_PURPOSE.as_bytes());
    hash.update([0]);
    hash.update(canonical_json(binding)?);
    Ok(format!("host-source:{}", hex::encode(hash.finalize())))
}

impl Journal {
    /// Read-only current database authorization for the separately held host
    /// source service. Token validity and scope equality do not create the
    /// issuer's held OS ownership, a RemoteSession or a checkpoint/history proof.
    /// Call off App with trusted host time and the already-established journal;
    /// this method never opens/initializes a database or claims ownership.
    pub fn authorize_host_source(
        &self,
        token: &str,
        binding: &PartialBinding,
        now_ms: i64,
    ) -> Result<HostSourceAuthorization> {
        let tx = self.conn.unchecked_transaction()?;
        let result = Self::authorize_host_source_transaction(&tx, token, binding, now_ms)?;
        tx.commit()?;
        Ok(result)
    }

    pub(super) fn authorize_host_source_transaction(
        tx: &Transaction<'_>,
        token: &str,
        binding: &PartialBinding,
        now_ms: i64,
    ) -> Result<HostSourceAuthorization> {
        let scope = host_source_scope(binding)?;
        let auth = authenticate(tx, token, now_ms)?;
        authorize(tx, &auth, HOST_SOURCE_REQUIRED_OPERATION)?;
        ensure!(
            auth.grant.scopes.contains(&scope),
            "host source is outside capability"
        );
        let attempt = &binding.attempt;
        ensure!(
            auth.project == attempt.project_id
                && auth.grant.run_id == attempt.run_id
                && auth.grant.run_epoch == attempt.run_epoch,
            "host source belongs to another project/run"
        );
        let project = project_state(tx, &auth.project)?;
        ensure!(
            project.owner_active
                && project.owner.owner_id == attempt.owner.owner
                && project.owner.epoch == attempt.owner.epoch,
            "host source owner changed"
        );
        let instance: Option<String> = tx.query_row(
            "SELECT instance_id FROM runs WHERE project_id=?1 AND run_id=?2",
            params![auth.project, auth.grant.run_id],
            |row| row.get(0),
        )?;
        ensure!(
            instance.as_deref() == Some(attempt.execution_id.as_str()),
            "host source execution changed"
        );
        let expires_at_ms = auth.grant.expires_at_ms;
        let identity = CapabilityIdentity {
            project_id: auth.project,
            principal: auth.grant.principal,
            run_id: auth.grant.run_id,
            run_epoch: auth.grant.run_epoch,
            owner_epoch: project.owner.epoch,
        };
        Ok(HostSourceAuthorization {
            identity,
            validated_at_ms: now_ms,
            expires_at_ms,
        })
    }
}
