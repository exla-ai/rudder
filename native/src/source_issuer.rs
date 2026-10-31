//! Off-App actual host owner + exact Journal adapter. No endpoint, capability,
//! accepted history or source session is created. Tokens stay in host memory.
use crate::{engine_host::EngineSourceLease, source_authorizer::HostSourceAuthority};
use anyhow::{ensure, Context, Result};
use rudder_engine::{journal::HostSourceAuthorization, vfs::partial_rpc::PartialBinding};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const QUERY_BUSY_TIMEOUT: Duration = Duration::from_millis(100);

/// Private capability storage, deliberately no Debug/Serde/display/accessor.
struct HostSourceToken(String);

/// A real owned callback, never reconstructed from records, a filename or PID.
/// The transport must retain its Arc until actual callback/TLS settlement and
/// keep this host-only service separate from guest broker ingress/rotation.
pub(crate) struct HostSourceIssuer {
    lease: EngineSourceLease,
    binding: PartialBinding,
    token: HostSourceToken,
    denied: AtomicBool,
}

impl HostSourceIssuer {
    /// Off-App preparation. Binding/token are expectations until an actual
    /// per-request Journal query succeeds. No alternate database can be supplied.
    pub(crate) fn new(
        lease: EngineSourceLease,
        binding: PartialBinding,
        token: String,
    ) -> Result<Arc<Self>> {
        binding.validate()?;
        ensure!(
            lease.owner().project_id == binding.attempt.project_id
                && lease.owner().owner_id == binding.attempt.owner.owner
                && lease.owner().epoch == binding.attempt.owner.epoch,
            "host source binding differs from the actual owner"
        );
        ensure!(
            token.len() == 64
                && token
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
            "invalid private host source capability"
        );
        lease.check_current()?;
        Ok(Arc::new(Self {
            lease,
            binding,
            token: HostSourceToken(token),
            denied: AtomicBool::new(false),
        }))
    }

    /// Off-App local held custody only; exact Journal/run authority is queried
    /// by authorize on every request. This check grants no source permission.
    pub(crate) fn check_current(&self) -> Result<()> {
        ensure!(
            !self.denied.load(Ordering::Acquire),
            "host source issuer is fenced"
        );
        self.lease.check_current()
    }

    fn authorize_using(
        &self,
        binding: &PartialBinding,
        query_time: impl FnOnce() -> Result<i64>,
    ) -> Result<HostSourceAuthorization> {
        let result = (|| {
            self.check_current()?;
            ensure!(binding == &self.binding, "host source binding changed");
            let journal = self.lease.open_journal(QUERY_BUSY_TIMEOUT)?;
            // Sample after potentially contended opening/setup, not at queue
            // admission. The transport separately samples again after callback.
            let now_ms = query_time()?;
            ensure!(now_ms >= 0, "trusted host source clock is uncertain");
            self.check_current()?;
            let authorization = journal.authorize_host_source(&self.token.0, binding, now_ms);
            // Recheck even when the query refused. No denial or late result may
            // silently renew this callback or resurrect another request.
            self.check_current()?;
            let authorization = authorization?;
            ensure!(
                authorization.identity.project_id == self.binding.attempt.project_id
                    && authorization.identity.run_id == self.binding.attempt.run_id
                    && authorization.identity.run_epoch == self.binding.attempt.run_epoch
                    && authorization.identity.owner_epoch == self.lease.owner().epoch
                    && authorization.validated_at_ms == now_ms,
                "host source Journal identity changed"
            );
            self.check_current()?;
            Ok(authorization)
        })();
        if result.is_err() {
            self.denied.store(true, Ordering::Release);
        }
        result
    }
}

fn trusted_query_time() -> Result<i64> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("trusted host source clock precedes epoch")?;
    i64::try_from(elapsed.as_millis()).context("trusted host source time is out of range")
}

impl HostSourceAuthority for HostSourceIssuer {
    fn authorize(&self, binding: &PartialBinding) -> Result<HostSourceAuthorization> {
        self.authorize_using(binding, trusted_query_time)
    }
}

#[cfg(test)]
#[path = "source_issuer_tests.rs"]
mod tests;

#[cfg(test)]
pub(crate) use tests::Fixture as SourceIssuerFixture;
