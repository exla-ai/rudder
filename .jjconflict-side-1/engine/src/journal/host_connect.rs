//! Host-only HTTPS CONNECT authorization. No endpoint, inventory witness, owner,
//! token issuance, model admission or Source5 protocol activation happens here.
use super::*;
use crate::{execution::network::connected::ConnectBinding, protocol::canonical_json};

pub const HOST_CONNECT_PURPOSE: &str = "https_connect_egress_v1";
pub const HOST_CONNECT_REQUIRED_OPERATION: &str = "network:host:connect";
const SCOPE_DOMAIN: &[u8] = b"rudder_host_connect_authorization_v1\0";

/// A transaction snapshot, never a live grant or reusable readiness receipt.
/// Adapters must recheck trusted time and original local deadline AFTER the
/// query, and must not derive a new deadline from this result's arrival time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostConnectAuthorization {
    pub identity: CapabilityIdentity,
    pub validated_at_ms: i64,
    pub issued_at_ms: i64,
    pub expires_at_ms: i64,
}

/// Domain-separated exact sealed configuration, including profile digest,
/// project/owner/run epochs, request, execution, operation, view and network.
/// The profile digest covers pinned destinations, budgets and readiness nonce.
pub fn host_connect_scope(binding: &ConnectBinding) -> Result<String> {
    binding.validate()?;
    let bytes = canonical_json(binding)?;
    ensure!(bytes.len() <= 4096, "host connect binding exceeds4KiB");
    let mut hash = Sha256::new();
    hash.update(SCOPE_DOMAIN);
    hash.update(HOST_CONNECT_PURPOSE.as_bytes());
    hash.update([0]);
    hash.update(bytes);
    Ok(format!("host-connect:{}", hex::encode(hash.finalize())))
}

/// A dedicated host principal. This grants no authority by itself.
pub fn host_connect_principal(binding: &ConnectBinding) -> Result<String> {
    binding.validate()?;
    Ok(format!("host-connect:{}", binding.network.request_id))
}

impl Journal {
    /// Fresh read-only capability/current-attempt authorization. expected_expiry
    /// is fixed by the actual issuer before adapter construction. Finite grants
    /// are mandatory; the same token cannot silently acquire a later expiry.
    pub fn authorize_host_connect(
        &self,
        token: &str,
        binding: &ConnectBinding,
        expected_expiry_ms: i64,
        now_ms: i64,
    ) -> Result<HostConnectAuthorization> {
        let tx = self.conn.unchecked_transaction()?;
        let result = Self::authorize_host_connect_transaction(
            &tx, token, binding, expected_expiry_ms, now_ms)?;
        tx.commit()?;
        Ok(result)
    }

    pub(super) fn authorize_host_connect_transaction(
        tx: &Transaction<'_>,
        token: &str,
        binding: &ConnectBinding,
        expected_expiry_ms: i64,
        now_ms: i64,
    ) -> Result<HostConnectAuthorization> {
        ensure!(now_ms >= 0 && expected_expiry_ms > now_ms,
            "host connect time or original grant expiry is invalid");
        let scope = host_connect_scope(binding)?;
        let auth = authenticate(tx, token, now_ms)?;
        authorize(tx, &auth, HOST_CONNECT_REQUIRED_OPERATION)?;
        // Host egress is a separate token, never an added guest tool operation.
        ensure!(auth.grant.operations.len() == 1
            && auth.grant.scopes.len() == 1 && auth.grant.scopes.contains(&scope)
            && auth.grant.recipients.is_empty() && auth.grant.topics.is_empty()
            && auth.grant.principal == host_connect_principal(binding)?
            && auth.grant.expires_at_ms == Some(expected_expiry_ms),
            "host connect requires its exact dedicated finite capability");
        let network = &binding.network;
        ensure!(auth.project == network.project_id
            && auth.grant.run_id == network.run_id
            && auth.grant.run_epoch == network.run_epoch.value(),
            "host connect belongs to another project/run");
        let project = project_state(tx, &auth.project)?;
        ensure!(project.owner_active && project.owner.owner_id == network.owner
            && project.owner.epoch == network.owner_epoch.value(),
            "host connect owner changed");
        let instance: Option<String> = tx.query_row(
            "SELECT instance_id FROM runs WHERE project_id=?1 AND run_id=?2",
            params![auth.project, auth.grant.run_id], |row| row.get(0))?;
        ensure!(instance.as_deref() == Some(network.execution_id.as_str()),
            "host connect execution changed");
        // Sample supplied by the trusted host must not precede actual issuance.
        let issued_at_ms: i64 = tx.query_row(
            "SELECT issued_at FROM capabilities WHERE token_hash=?1",
            [token_hash(token)], |row| row.get(0))?;
        ensure!(issued_at_ms >= 0 && now_ms >= issued_at_ms,
            "host connect clock precedes capability issuance");
        Ok(HostConnectAuthorization {
            identity: CapabilityIdentity { project_id: auth.project,
                principal: auth.grant.principal, run_id: auth.grant.run_id,
                run_epoch: auth.grant.run_epoch, owner_epoch: project.owner.epoch },
            validated_at_ms: now_ms, issued_at_ms, expires_at_ms: expected_expiry_ms,
        })
    }
}
