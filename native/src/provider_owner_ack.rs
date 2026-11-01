//! Only the authenticated SAME original Root exchange issues these owners.
//! The15s budget is LOCAL liveness, never a globally expiring owner grant.
use super::*;
pub struct OriginalProviderAcquireAck {registration:Arc<RegisteredEngineOwner>,ack:EngineOperationAck,liveness:FinalizationBudget,owner_receipt:serde_json::Value,published:AtomicBool,claimed:AtomicBool}
impl OriginalProviderAcquireAck {
 pub(crate) fn claim_owner_retaining(&self)->Result<()>{ensure!(!self.claimed.swap(true,Ordering::AcqRel),"provider original ACK already consumed by retained owner attempt");self.assert_current()}
 pub fn assert_current(&self)->Result<()>{ensure!(self.published.load(Ordering::Acquire),"original provider acquisition ACK absent");self.liveness.remaining()?;self.registration.assert_current()}
 pub fn registration(&self)->&Arc<RegisteredEngineOwner>{&self.registration}pub fn receipt_data(&self)->&EngineOperationAck{&self.ack}pub fn owner_receipt(&self)->&serde_json::Value{&self.owner_receipt}
 pub fn original_liveness_budget(&self)->&FinalizationBudget{&self.liveness}
}
pub struct OriginalProviderHeartbeatAck {registration:Arc<RegisteredEngineOwner>,ack:EngineOperationAck,liveness:FinalizationBudget,grant:serde_json::Value,published:AtomicBool}
impl OriginalProviderHeartbeatAck {
 pub fn assert_current(&self)->Result<()>{ensure!(self.published.load(Ordering::Acquire),"original provider heartbeat ACK absent");self.liveness.remaining()?;self.registration.assert_current()}
 pub fn registration(&self)->&Arc<RegisteredEngineOwner>{&self.registration}pub fn receipt_data(&self)->&EngineOperationAck{&self.ack}pub fn owner_grant(&self)->&serde_json::Value{&self.grant}
 pub fn original_liveness_budget(&self)->&FinalizationBudget{&self.liveness}
}
impl EngineOperation {
 pub fn take_original_provider_acquire_ack_retaining(&mut self,out:&mut Option<Arc<OriginalProviderAcquireAck>>)->Result<bool>{
  ensure!(out.is_none(),"provider acquisition output occupied");if !self.poll_retaining()?{return Ok(false)}let ack=self.receipt.as_ref().unwrap();
  let EngineOperationKind::AcquireProviderFreshOwnerV1 {workspace_id,transfer_id,acquire_key,..}=&ack.operation else{return Err(fault())};ensure!(ack.error.is_none(),"provider first acquire rejected");
  #[derive(Deserialize)]#[serde(deny_unknown_fields)]struct Data{scope:String,state:String,transfer_id:String,project_id:String,workspace_id:String,acquire_key:String,owner_receipt:serde_json::Value}
  let data:Data=serde_json::from_value(ack.result.clone().context("provider first grant absent")?).map_err(|_|fault())?;
  let liveness=self.acquire_clock.as_ref().context("provider original first-send liveness clock absent")?.clone();
  *out=Some(Arc::new(OriginalProviderAcquireAck{registration:self.registration.clone(),ack:ack.clone(),liveness,owner_receipt:data.owner_receipt,published:AtomicBool::new(false),claimed:AtomicBool::new(false)}));
  let owner=out.as_ref().unwrap();ensure!(data.scope=="fresh-provider-owner-acquire-only"&&data.state=="acquired"&&data.transfer_id==*transfer_id&&data.workspace_id==*workspace_id&&data.acquire_key==*acquire_key,"provider original first acquire result differs");
  let accepted:provider_data::ProviderAccepted=serde_json::from_value(owner.owner_receipt.clone()).map_err(|_|fault())?;accepted.validate_data(self.registration.data())?;
  ensure!(accepted.data.project_id==data.project_id&&accepted.data.request_id==*acquire_key&&accepted.data.heartbeat_serial=="0","provider first grant binding differs");
  self.time()?;owner.liveness.remaining()?;self.registration.assert_current()?;self.finish_known_retaining()?;out.as_ref().unwrap().published.store(true,Ordering::Release);Ok(true)
 }
 pub fn take_original_provider_heartbeat_ack_retaining(&mut self,out:&mut Option<Arc<OriginalProviderHeartbeatAck>>)->Result<bool>{
  ensure!(out.is_none(),"provider heartbeat output occupied");if !self.poll_retaining()?{return Ok(false)}let ack=self.receipt.as_ref().unwrap();
  let EngineOperationKind::HeartbeatProviderOwnerV1 {workspace_id,owner_epoch,ownership_id,request_id,expected_serial}=&ack.operation else{return Err(fault())};ensure!(ack.error.is_none(),"provider heartbeat rejected");
  #[derive(Deserialize)]#[serde(deny_unknown_fields)]struct Data{scope:String,state:String,project_id:String,workspace_id:String,request_id:String,owner_grant:serde_json::Value}
  let data:Data=serde_json::from_value(ack.result.clone().context("provider heartbeat grant absent")?).map_err(|_|fault())?;let liveness=self.renewal_clock.as_ref().context("provider original first-send heartbeat clock absent")?.clone();
  *out=Some(Arc::new(OriginalProviderHeartbeatAck{registration:self.registration.clone(),ack:ack.clone(),liveness,grant:data.owner_grant,published:AtomicBool::new(false)}));
  let owner=out.as_ref().unwrap();let grant:provider_data::ProviderGrant=serde_json::from_value(owner.grant.clone()).map_err(|_|fault())?;grant.validate_data(self.registration.data())?;
  ensure!(data.scope=="provider-owner-heartbeat-only"&&data.state=="acknowledged"&&data.workspace_id==*workspace_id&&data.project_id==grant.project_id&&data.request_id==*request_id&&grant.request_id==*request_id&&grant.ownership_id==*ownership_id&&grant.owner_epoch==*owner_epoch
   &&rudder_engine::fleet_lease::Counter::parse(&grant.heartbeat_serial)?.value()==rudder_engine::fleet_lease::Counter::parse(expected_serial)?.value().checked_add(1).context("provider heartbeat serial exhausted")?,"provider original heartbeat differs");
  self.time()?;owner.liveness.remaining()?;self.registration.assert_current()?;self.finish_known_retaining()?;out.as_ref().unwrap().published.store(true,Ordering::Release);Ok(true)
 }
}
