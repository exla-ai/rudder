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
    /// Bounded local check on App adoption. Keep this value through settlement;
    /// never convert its existence into accepted history or deployment proof.
    pub(crate) fn provisional(&self) -> Result<&ProvisionalSourceShutdown> {
        self.authorization
            .check_adoption(&self.joined.issuer, self.joined.provisional.budget())?;
        Ok(&self.joined.provisional)
    }
}
