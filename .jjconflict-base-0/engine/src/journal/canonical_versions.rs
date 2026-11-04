//! Original Journal snapshot of ALL accepted decisions, not caller-selected
//! topics. This is local policy DATA custody, never a Root/PG/execution grant.
use super::*;
use crate::execution::finalization_budget::FinalizationBudget;
use std::sync::atomic::{AtomicBool,Ordering};
pub const CANONICAL_DECISION_SET_TOPIC:&str="__rudder_canonical_decision_set_v1";
pub const CANONICAL_BUILD_POLICY_TOPIC:&str="build-policy/canonical";
const MAX_TOPICS:usize=1024;
pub struct CanonicalDecisionSnapshot{owner:OwnerFence,head:String,versions:TopicVersions,policy:Option<AcceptedDecision>}
fn time(original:&FinalizationBudget,cancelled:&AtomicBool)->Result<()>{if cancelled.load(Ordering::Acquire){original.cancel();}original.remaining()?;Ok(())}
pub(super) fn decision_set_revision(conn:&Connection,project:&str)->Result<u64>{
    conn.query_row("SELECT coalesce(MAX(sequence),0) FROM decisions WHERE project_id=?1",[project],|row|db_u64(row,0)).map_err(Into::into)
}
fn snapshot(conn:&Connection,owner:&OwnerFence,head:&str)->Result<(TopicVersions,Option<AcceptedDecision>)>{
    assert_owner(conn,owner)?;ensure!(project_state(conn,&owner.project_id)?.integration_head==head,"canonical accepted head changed");
    let mut statement=conn.prepare("SELECT topic,version,decision_id FROM topics WHERE project_id=?1 ORDER BY topic LIMIT ?2")?;
    let rows=statement.query_map(params![owner.project_id,(MAX_TOPICS+1)as i64],|r|Ok((r.get::<_,String>(0)?,db_u64(r,1)?,r.get::<_,String>(2)?)))?;
    let mut versions=BTreeMap::new();let mut policy=None;
    for row in rows{let(topic,version,id)=row?;ensure!(versions.len()<MAX_TOPICS&&topic!=CANONICAL_DECISION_SET_TOPIC,"canonical topic bound/reserved topic conflict");
        if topic==CANONICAL_BUILD_POLICY_TOPIC{policy=Some(decision(conn,&id)?);}ensure!(versions.insert(topic,version).is_none(),"canonical topic duplicate");}
    versions.entry(CANONICAL_BUILD_POLICY_TOPIC.into()).or_insert(0);
    versions.insert(CANONICAL_DECISION_SET_TOPIC.into(),decision_set_revision(conn,&owner.project_id)?);Ok((versions,policy))
}
impl Journal{
    /// Capture BEFORE canonical execution. MAX(decisions.sequence) also changes
    /// when an older writer creates a previously absent topic; no counter/schema
    /// migration or cooperation from that writer is assumed.
    pub fn capture_canonical_decisions(&self,owner:&OwnerFence,head:&str,original:&FinalizationBudget,cancelled:&AtomicBool)->Result<CanonicalDecisionSnapshot>{
        time(original,cancelled)?;let tx=self.conn.unchecked_transaction()?;let(versions,policy)=snapshot(&tx,owner,head)?;tx.commit()?;time(original,cancelled)?;
        Ok(CanonicalDecisionSnapshot{owner:owner.clone(),head:head.into(),versions,policy})
    }
}
impl CanonicalDecisionSnapshot{
    pub fn owner_data(&self)->&OwnerFence{&self.owner}
    pub fn head_data(&self)->&str{&self.head}
    pub fn versions_data(&self)->&TopicVersions{&self.versions}
    pub fn policy_decision_data(&self)->Option<&AcceptedDecision>{self.policy.as_ref()}
    pub fn assert_current(&self,journal:&Journal,original:&FinalizationBudget,cancelled:&AtomicBool)->Result<()>{
        time(original,cancelled)?;let tx=journal.conn.unchecked_transaction()?;let(actual,policy)=snapshot(&tx,&self.owner,&self.head)?;
        ensure!(actual==self.versions&&policy==self.policy,"canonical accepted decision/build-policy set changed");tx.commit()?;time(original,cancelled)
    }
}
#[cfg(test)]
#[path="canonical_versions_tests.rs"]mod tests;
