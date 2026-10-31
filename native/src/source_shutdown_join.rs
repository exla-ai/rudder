//! Process-local join only; no JSON, PID, status or saved authority constructor.
//! App owns the terminal. Its bounded worker owns this value during Journal IO.
use crate::source_issuer::{HostSourceIssuer, SourceShutdownAuthorization};
use anyhow::Result;
use rudder_engine::execution::handoff::source_shutdown::ProvisionalSourceShutdown;
use rudder_native::pty_terminal::{ManagedHelperCustody, SuccessfulManagedHelperExit};
use std::sync::Arc;

pub(crate) struct ExitedSourceShutdown {
    provisional: ProvisionalSourceShutdown,
    issuer: Arc<HostSourceIssuer>,
    _exit: SuccessfulManagedHelperExit,
    _custody: ManagedHelperCustody,
}
pub(crate) struct JoinedSourceShutdown {
    joined: ExitedSourceShutdown,
    authorization: SourceShutdownAuthorization,
}

impl ExitedSourceShutdown {
    /// Validate by borrow before moving the exact exit witnesses. An error or
    /// unwinding authorization caller must retain these original owning slots.
    pub(crate) fn new_retaining(
        provisional: &mut Option<ProvisionalSourceShutdown>,
        issuer: Arc<HostSourceIssuer>,
        custody: &mut Option<ManagedHelperCustody>,
        exit: &mut Option<SuccessfulManagedHelperExit>,
    ) -> Result<Self> {
        let pending = provisional.as_ref().ok_or_else(||anyhow::anyhow!("source provisional exit absent"))?;
        let actual_custody = custody.as_ref().ok_or_else(||anyhow::anyhow!("source helper custody absent"))?;
        let actual_exit = exit.as_ref().ok_or_else(||anyhow::anyhow!("source helper exit absent"))?;
        pending.budget().remaining()?;
        actual_exit.matches(actual_custody, pending)?;
        Ok(Self { provisional: provisional.take().unwrap(), issuer,
            _exit: exit.take().unwrap(), _custody: custody.take().unwrap() })
    }

    /// Off-App authorization borrows the original packet. Query failure or
    /// panic leaves the actual exit/custody/issuer owned by its outer carrier.
    pub(crate) fn join_retaining(pending: &mut Option<Self>) -> Result<JoinedSourceShutdown> {
        let exited = pending.as_ref().ok_or_else(||anyhow::anyhow!("source exit packet absent"))?;
        let authorization = exited.issuer.authorize_shutdown_join(
            exited.provisional.binding(), exited.provisional.budget())?;
        Ok(JoinedSourceShutdown { joined: pending.take().unwrap(), authorization })
    }

    /// App calls after the exact owned child has actually been reaped. The
    /// witness constructor and its status stay private to pty_terminal.
    pub(crate) fn new(
        provisional: ProvisionalSourceShutdown,
        issuer: Arc<HostSourceIssuer>,
        custody: ManagedHelperCustody,
        exit: SuccessfulManagedHelperExit,
    ) -> Result<Self> {
        provisional.budget().remaining()?;
        exit.matches(&custody, &provisional)?;
        Ok(Self {
            provisional,
            issuer,
            _exit: exit,
            _custody: custody,
        })
    }

    /// Off-App only. Queue delay and actual Journal IO consume the SAME budget.
    /// The issuer Arc retains EngineSourceLease until this worker truly settles,
    /// even if the App times out/cancels/drops its result receiver meanwhile.
    pub(crate) fn join(self) -> Result<JoinedSourceShutdown> {
        let authorization = self
            .issuer
            .authorize_shutdown_join(self.provisional.binding(), self.provisional.budget())?;
        Ok(JoinedSourceShutdown {
            joined: self,
            authorization,
        })
    }
}
impl JoinedSourceShutdown {
    /// Identity of the actual issuer retained through the successful join.
    pub(crate) fn matches_issuer(&self, issuer: &Arc<HostSourceIssuer>) -> bool {
        Arc::ptr_eq(&self.joined.issuer, issuer)
    }
    /// Bounded local check on App adoption. Keep this value through settlement;
    /// never convert its existence into accepted history or deployment proof.
    pub(crate) fn provisional(&self) -> Result<&ProvisionalSourceShutdown> {
        self.authorization
            .check_adoption(&self.joined.issuer, self.joined.provisional.budget())?;
        Ok(&self.joined.provisional)
    }
}
