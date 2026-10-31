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
#[cfg(target_os = "linux")]
const SHUTDOWN_QUERY_FRESHNESS: Duration = Duration::from_secs(3);

/// Only the actual final query constructs this local adoption gate. Its true
/// grant expiry and freshness stay bounded by the ORIGINAL suspend-aware clock.
#[cfg(target_os = "linux")]
pub(crate) struct SourceShutdownAuthorization {
    remaining_at_query_start: Duration,
    valid_for: Duration,
    after_query_ms: i64,
    expires_at_ms: Option<i64>,
}
#[cfg(target_os = "linux")]
impl SourceShutdownAuthorization {
    /// App-safe clocks/atomics only. This is bounded result adoption, not an
    /// atomic Journal commit or protection against a subsequent revocation.
    pub(crate) fn check_adoption(&self, issuer: &HostSourceIssuer,
        budget: &rudder_engine::execution::finalization_budget::FinalizationBudget) -> Result<()> {
        let remaining = budget.remaining()?;
        let age = self.remaining_at_query_start.checked_sub(remaining)
            .context("shutdown query clock changed")?;
        ensure!(age < self.valid_for, "shutdown query is no longer fresh");
        let now = trusted_query_time()?;
        ensure!(now >= self.after_query_ms && self.expires_at_ms.is_none_or(|expiry| now < expiry),
            "shutdown host source authorization expired or clock regressed");
        ensure!(!issuer.denied.load(Ordering::Acquire), "host source issuer is fenced");
        issuer.lease.check_active_for_adoption()?;
        budget.remaining()?;
        Ok(())
    }
}

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
    /// Final join must query the actual Journal again, after the parent's owned
    /// helper wait. A transport freshness receipt/snapshot never substitutes.
    /// Off-App; caller retains this Arc and the original budget through result
    /// settlement. A delayed query cannot renew either time or host grant.
    #[cfg(target_os = "linux")]
    pub(crate) fn authorize_shutdown_join(
        &self,
        binding: &PartialBinding,
        budget: &rudder_engine::execution::finalization_budget::FinalizationBudget,
    ) -> Result<SourceShutdownAuthorization> {
        let remaining_at_query_start = budget.remaining()?;
        let authorization = self.authorize_using(binding, trusted_query_time)?;
        budget.remaining()?;
        let after = trusted_query_time()?;
        ensure!(after >= authorization.validated_at_ms
            && authorization.expires_at_ms.is_none_or(|expiry| after < expiry),
            "host source grant expired or clock changed during shutdown join");
        self.check_current()?;
        budget.remaining()?;
        let valid_for = match authorization.expires_at_ms {
            Some(expiry) => Duration::from_millis(u64::try_from(expiry.checked_sub(authorization.validated_at_ms)
                .context("shutdown grant expiry overflow")?).context("shutdown grant expired")?)
                .min(SHUTDOWN_QUERY_FRESHNESS),
            None => SHUTDOWN_QUERY_FRESHNESS,
        };
        let result = SourceShutdownAuthorization { remaining_at_query_start, valid_for,
            after_query_ms: after, expires_at_ms: authorization.expires_at_ms };
        result.check_adoption(self, budget)?;
        Ok(result)
    }
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
