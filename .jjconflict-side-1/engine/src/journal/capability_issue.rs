//! Trusted host-only, process-local issuance ownership. No serde/token injection
//! or saved reconstruction. Every current observation is DATA, never first ACK.
use super::*;
use std::{fmt, path::PathBuf};

#[derive(Debug, PartialEq, Eq)]
enum Phase { Prepared, Attempted, Acknowledged, Retired }
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityIssuanceObservation { Absent, Present, Revoked }

/// Put this owner in the retained carrier BEFORE invoking the write. Err/panic
/// never consumes it; the private token remains usable for exact reconciliation
/// and retirement, even when no original commit acknowledgment was observed.
#[must_use = "retain until exact absent/revoked retirement is confirmed"]
pub struct PreparedCapabilityIssuance {
    owner: OwnerFence, grant: CapabilityGrant, grant_json: String, issued_at: i64,
    token: String, anchor: HeldJournalFile, path: PathBuf, phase: Phase,
}
impl fmt::Debug for PreparedCapabilityIssuance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PreparedCapabilityIssuance").field("secret", &"[REDACTED]")
            .field("phase", &self.phase).finish()
    }
}
impl fmt::Display for PreparedCapabilityIssuance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "{self:?}") }
}
impl PreparedCapabilityIssuance {
    /// Token DATA after the original commit actually returned, not proof of
    /// current authority. A Present observation cannot enable this accessor.
    pub fn acknowledged_token(&self) -> Result<&str> {
        ensure!(self.phase == Phase::Acknowledged, "original issuance ACK unavailable");
        Ok(&self.token)
    }
    pub fn is_retired(&self) -> bool { self.phase == Phase::Retired }
    fn check_journal(&self, journal: &Journal) -> Result<()> {
        self.anchor.check_current()?;
        ensure!(journal.database_path()? == self.path.as_path(), "issuance belongs to another Journal");
        journal.hold_database_file()?.check_current()?;
        self.anchor.check_current()
    }
    fn current(&self, conn: &Connection) -> Result<()> {
        assert_owner(conn, &self.owner)?;
        assert_run(conn, &self.owner.project_id, &self.grant.run_id, self.grant.run_epoch)
    }
    fn observe(&self, conn: &Connection) -> Result<CapabilityIssuanceObservation> {
        let row: Option<(String,String,bool,i64)> = conn.query_row(
            "SELECT project_id,substr(grant_json,1,65537),revoked,issued_at FROM capabilities WHERE token_hash=?1",
            [token_hash(&self.token)], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
        match row {
            None => Ok(CapabilityIssuanceObservation::Absent),
            Some((project, grant, revoked, issued)) => {
                ensure!(project == self.owner.project_id && grant == self.grant_json && issued == self.issued_at,
                    "retained issuance row differs from exact original grant/time");
                Ok(if revoked { CapabilityIssuanceObservation::Revoked } else { CapabilityIssuanceObservation::Present })
            }
        }
    }
}
impl Journal {
    /// Off-App preparation only: private file custody and owner/run validation,
    /// with no capability insertion. Original grant/expiry/time never change.
    pub fn prepare_capability_issuance(&self, owner: &OwnerFence, grant: &CapabilityGrant,
        original_issued_at_ms: i64) -> Result<PreparedCapabilityIssuance> {
        valid_id(&grant.principal)?; valid_id(&grant.run_id)?;
        ensure!(original_issued_at_ms >= 0 && !grant.operations.is_empty()
            && grant.expires_at_ms.is_none_or(|at|at > original_issued_at_ms), "invalid prepared capability");
        let grant_json = json(grant)?;
        ensure!(grant_json.len() <= 64 * 1024, "prepared grant exceeds64KiB");
        let anchor = self.hold_database_file()?;
        let tx = self.conn.unchecked_transaction()?;
        assert_owner(&tx, owner)?; assert_run(&tx, &owner.project_id, &grant.run_id, grant.run_epoch)?;
        for (run, epoch) in &grant.recipients { assert_run(&tx, &owner.project_id, run, *epoch)?; }
        tx.commit()?; anchor.check_current()?;
        Ok(PreparedCapabilityIssuance { owner:owner.clone(), grant:grant.clone(), grant_json,
            issued_at:original_issued_at_ms, token:format!("{}{}",Uuid::new_v4().simple(),Uuid::new_v4().simple()),
            anchor, path:self.database_path()?.to_path_buf(), phase:Phase::Prepared })
    }
    /// One-shot, including transaction-open failures. Unknown inserts are never
    /// retried; use observe/retire on the SAME owner. No token crosses an Err.
    pub fn issue_prepared_capability(&mut self, prepared: &mut PreparedCapabilityIssuance,
        now_ms: i64) -> Result<()> { self.issue_prepared_inner(prepared, now_ms, || Ok(())) }
    fn issue_prepared_inner(&mut self, p: &mut PreparedCapabilityIssuance, now_ms: i64,
        before_ack: impl FnOnce() -> Result<()>) -> Result<()> {
        ensure!(p.phase == Phase::Prepared, "prepared issuance already attempted or retired");
        p.phase = Phase::Attempted; // Absorbing even if time/custody/transaction setup fails.
        ensure!(now_ms >= p.issued_at && p.grant.expires_at_ms.is_none_or(|at|at > now_ms),
            "prepared capability expired or clock regressed");
        p.check_journal(self)?;
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        p.current(&tx)?;
        for (run, epoch) in &p.grant.recipients { assert_run(&tx, &p.owner.project_id, run, *epoch)?; }
        let digest = token_hash(&p.token);
        tx.execute("INSERT INTO capabilities(token_hash,project_id,grant_json,revoked,issued_at) VALUES(?1,?2,?3,0,?4)",
            params![digest,p.owner.project_id,p.grant_json,p.issued_at])?;
        append_event(&tx, &p.owner.project_id, "capability-issued", &p.owner.owner_id,
            &serde_json::json!({"token_hash":digest,"grant":p.grant}), p.issued_at)?;
        tx.commit()?;
        before_ack()?; // Private seam for actual committed-but-unacknowledged tests.
        p.check_journal(self)?;
        p.phase = Phase::Acknowledged;
        Ok(())
    }
    /// Exact current DATA under the original current owner/run. Never restores
    /// first ACK, enables acknowledged_token, writes, or changes expiry.
    pub fn observe_prepared_capability(&self, p: &PreparedCapabilityIssuance) -> Result<CapabilityIssuanceObservation> {
        p.check_journal(self)?;
        let tx = self.conn.unchecked_transaction()?;
        p.current(&tx)?; let observation = p.observe(&tx)?;
        tx.commit()?; p.check_journal(self)?;
        Ok(observation)
    }
    /// Explicit idempotent cleanup. Absent and already-revoked are confirmed
    /// exactly; present matching rows are revoked in the same owner/run transaction.
    /// Even a never-inserted owner becomes permanently non-issuable on success.
    pub fn retire_prepared_capability(&mut self, p: &mut PreparedCapabilityIssuance,
        now_ms: i64) -> Result<CapabilityIssuanceObservation> {
        if p.phase != Phase::Retired { p.phase = Phase::Attempted; } // Cleanup also forbids later issuance.
        ensure!(now_ms >= p.issued_at, "issuance retirement clock regressed");
        p.check_journal(self)?;
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        p.current(&tx)?; let observation = p.observe(&tx)?;
        let result = if observation == CapabilityIssuanceObservation::Present {
            ensure!(tx.execute("UPDATE capabilities SET revoked=1 WHERE token_hash=?1 AND project_id=?2 AND revoked=0",
                params![token_hash(&p.token),p.owner.project_id])? == 1, "issuance changed during retirement");
            append_event(&tx, &p.owner.project_id, "capability-revoked", &p.owner.owner_id,
                &serde_json::json!({"token_hash":token_hash(&p.token)}), now_ms)?;
            CapabilityIssuanceObservation::Revoked
        } else { observation };
        tx.commit()?; p.check_journal(self)?;
        p.phase = Phase::Retired;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (tempfile::TempDir, Journal, OwnerFence, CapabilityGrant) {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut journal = Journal::open(dir.path().canonicalize().unwrap().join("journal.sqlite")).unwrap();
        std::fs::set_permissions(journal.database_path().unwrap(), std::fs::Permissions::from_mode(0o600)).unwrap();
        let owner = journal.create_project("p", "owner", "base", 10).unwrap();
        journal.register_run(&owner,"run",10).unwrap();
        let grant = CapabilityGrant { principal:"agent".into(), run_id:"run".into(), run_epoch:1,
            operations:BTreeSet::from(["agents:find".into()]), recipients:BTreeMap::new(), scopes:BTreeSet::new(),
            topics:BTreeSet::new(), expires_at_ms:Some(100) };
        (dir,journal,owner,grant)
    }
    #[test]
    fn prepared_issuance_exposes_only_acknowledged_token_and_preserves_old_api() {
        let (_dir,mut j,o,g) = fixture(); let mut p=j.prepare_capability_issuance(&o,&g,10).unwrap();
        assert!(p.acknowledged_token().is_err());
        assert!(!format!("{p:?}/{p}").contains(&p.token));
        assert_eq!(j.observe_prepared_capability(&p).unwrap(),CapabilityIssuanceObservation::Absent);
        j.issue_prepared_capability(&mut p,11).unwrap();
        j.find_agents(p.acknowledged_token().unwrap(),12).unwrap();
        assert_eq!(j.retire_prepared_capability(&mut p,101).unwrap(),CapabilityIssuanceObservation::Revoked);
        assert!(p.is_retired()); assert!(p.acknowledged_token().is_err());
        assert!(j.issue_prepared_capability(&mut p,12).is_err());
        let old=j.issue_capability(&o,&g,11).unwrap(); j.find_agents(&old,12).unwrap();
    }
    #[test]
    fn committed_error_and_panic_retain_identity_without_reconstructing_first_ack() {
        let (_dir,mut j,o,g)=fixture();
        for panic in [false,true] {
            let mut p=j.prepare_capability_issuance(&o,&g,10).unwrap(); let digest=token_hash(&p.token);
            let result=std::panic::catch_unwind(std::panic::AssertUnwindSafe(||j.issue_prepared_inner(&mut p,11,|| {
                if panic { panic!("injected after actual commit"); } bail!("lost original ACK")
            })));
            assert!(!matches!(result,Ok(Ok(())))); assert!(token_hash(&p.token)==digest);
            assert_eq!(j.observe_prepared_capability(&p).unwrap(),CapabilityIssuanceObservation::Present);
            assert!(p.acknowledged_token().is_err()); assert!(j.issue_prepared_capability(&mut p,12).is_err());
            assert_eq!(j.retire_prepared_capability(&mut p,12).unwrap(),CapabilityIssuanceObservation::Revoked);
            assert_eq!(j.retire_prepared_capability(&mut p,13).unwrap(),CapabilityIssuanceObservation::Revoked);
            let count:i64=j.conn.query_row("SELECT count(*) FROM events WHERE kind='capability-revoked' AND json_extract(payload_json,'$.token_hash')=?1",[digest],|r|r.get(0)).unwrap();
            assert_eq!(count,1);
        }
    }
    #[test]
    fn absent_retirement_is_absorbing_and_delayed_issuance_cannot_renew_expiry() {
        let (_dir,mut j,o,g)=fixture();let mut p=j.prepare_capability_issuance(&o,&g,10).unwrap();
        assert!(j.issue_prepared_capability(&mut p,100).is_err());
        assert_eq!(j.retire_prepared_capability(&mut p,101).unwrap(),CapabilityIssuanceObservation::Absent);
        assert_eq!(j.retire_prepared_capability(&mut p,102).unwrap(),CapabilityIssuanceObservation::Absent);
        assert!(j.issue_prepared_capability(&mut p,11).is_err());
    }
    #[test]
    fn wrong_journal_grant_time_owner_and_run_cannot_be_adopted_or_revoked() {
        let (_dir,mut j,o,g)=fixture();let mut p=j.prepare_capability_issuance(&o,&g,10).unwrap();
        let (_other_dir,mut other,_,_)=fixture();
        let mut wrong=j.prepare_capability_issuance(&o,&g,10).unwrap();
        assert!(other.issue_prepared_capability(&mut wrong,11).is_err());
        assert!(other.retire_prepared_capability(&mut wrong,11).is_err());
        assert_eq!(j.retire_prepared_capability(&mut wrong,11).unwrap(),CapabilityIssuanceObservation::Absent);
        j.issue_prepared_capability(&mut p,11).unwrap();
        j.conn.execute("UPDATE capabilities SET issued_at=12 WHERE token_hash=?1",[token_hash(&p.token)]).unwrap();
        assert!(j.observe_prepared_capability(&p).is_err()); assert!(j.retire_prepared_capability(&mut p,12).is_err());
        j.conn.execute("UPDATE capabilities SET issued_at=10,grant_json='{}' WHERE token_hash=?1",[token_hash(&p.token)]).unwrap();
        assert!(j.observe_prepared_capability(&p).is_err()); assert!(j.retire_prepared_capability(&mut p,12).is_err());
        j.conn.execute("UPDATE capabilities SET grant_json=?1 WHERE token_hash=?2",params![p.grant_json,token_hash(&p.token)]).unwrap();
        p.owner.epoch+=1;assert!(j.observe_prepared_capability(&p).is_err());p.owner.epoch-=1;
        j.advance_run_epoch(&o,"run",1,12).unwrap();
        assert!(j.observe_prepared_capability(&p).is_err());assert!(j.retire_prepared_capability(&mut p,12).is_err());
    }
}
