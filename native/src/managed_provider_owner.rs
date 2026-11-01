//! Provider-fenced ownership inside the existing admission surface. The local
//! heartbeat cutoff limits liveness only; the durable PG owner NEVER expires.
use super::*;
use crate::{engine_registration::{OriginalProviderAcquireAck,OriginalProviderHeartbeatAck,ProviderGrant},host_ingress::HostOwnerMode};
struct ProviderPending {request_id:String,expected_serial:String,operation:Option<EngineOperation>,ack:Option<Arc<OriginalProviderHeartbeatAck>>,transport:FinalizationBudget,cancelled:Arc<AtomicBool>}
pub(super) struct ProviderState {grant:ProviderGrant,budget:FinalizationBudget,heartbeat:Option<Arc<OriginalProviderHeartbeatAck>>,pending:Option<ProviderPending>}
impl ManagedOwnerLease {
 pub fn capture_provider_retaining(out:&mut Option<Arc<Self>>,initial:Arc<OriginalProviderAcquireAck>,config:Arc<HostIngressConfigOwner>)->Result<()>{
  ensure!(out.is_none(),"provider owner output occupied");let workspace=initial.receipt_data().operation.workspace_id().to_owned();
  *out=Some(Arc::new(Self{initial:None,provider_initial:Some(initial),provider_state:Mutex::new(None),config,safety:None,workspace,identity:OnceLock::new(),state:Mutex::new(None),clock:None,pid:std::process::id(),last:AtomicU64::new(0),deadline:AtomicU64::new(0),serving:AtomicBool::new(false),adopting:AtomicBool::new(true),fenced:Arc::new(AtomicBool::new(false)),watchdog_started:AtomicBool::new(false)}));
  let result=(||->Result<()>{out.as_ref().unwrap().provider_initial.as_ref().unwrap().claim_owner_retaining()?;let clock=HostClock::new().map_err(|_|fault())?;Arc::get_mut(out.as_mut().unwrap()).context("provider owner escaped staging")?.clock=Some(clock);out.as_ref().unwrap().capture_provider_inner()})();if result.is_err(){out.as_ref().unwrap().fence();}result
 }
 fn capture_provider_inner(&self)->Result<()>{let initial=self.provider_initial.as_ref().context("actual provider original ACK absent")?;initial.assert_current()?;self.config.assert_current()?;
  ensure!(self.config.data().owner_mode==Some(HostOwnerMode::ProviderFencedV1)&&self.config.data().owner_lease_safety.is_none()&&self.config.config_sha256()==initial.registration().data().binding.config_sha256,"provider original config/mode differs");
  let grant:ProviderGrant=serde_json::from_value(initial.owner_receipt().get("data").context("provider grant absent")?.clone()).map_err(|_|fault())?;grant.validate_data(initial.registration().data())?;
  let EngineOperationKind::AcquireProviderFreshOwnerV1{acquire_key,expected_owner_epoch,..}=&initial.receipt_data().operation else{return Err(fault())};
  ensure!(expected_owner_epoch=="1"&&grant.request_id==*acquire_key&&grant.heartbeat_serial=="0"&&self.config.data().projects.iter().any(|p|p.alias==self.workspace&&p.prepare.namespace.project_id==grant.project_id),"provider original acquisition/project differs");
  let budget=initial.original_liveness_budget().clone();let(deadline,now)=budget.observed_clock_bounds()?;ensure!(now<deadline,"provider original LOCAL heartbeat cutoff lost");
  self.identity.set(IdentityData{project_id:grant.project_id.clone(),owner_epoch:grant.owner_epoch.clone()}).map_err(|_|fault())?;
  *self.provider_state.lock().map_err(|_|fault())?=Some(ProviderState{grant,budget,heartbeat:None,pending:None});self.last.store(now,Ordering::Release);self.deadline.store(deadline,Ordering::Release);
  initial.assert_current()?;self.config.assert_current()?;ensure!(self.sample()?<deadline,"provider local heartbeat cutoff lost during adoption");self.serving.store(true,Ordering::Release);self.adopting.store(false,Ordering::Release);Ok(())
 }
 pub(super) fn provider_current_off_app(&self)->Result<()>{self.registration().assert_current()?;self.config.assert_current()?;
  let state=match self.provider_state.try_lock(){Ok(v)=>v,Err(std::sync::TryLockError::WouldBlock)=>return Err(OwnerAdmissionBusy.into()),Err(_)=>return Err(fault())};let state=state.as_ref().context("provider original state absent")?;state.budget.remaining()?;Ok(())}
 pub(super) fn poll_provider_heartbeat_off_app(&self)->Result<()>{self.assert_current_off_app()?;
  let mut held=match self.provider_state.try_lock(){Ok(v)=>v,Err(std::sync::TryLockError::WouldBlock)=>return Err(OwnerAdmissionBusy.into()),Err(_)=>return Err(fault())};let state=held.as_mut().context("provider original state absent")?;
  let now=self.sample()?;let old=self.deadline.load(Ordering::Acquire);ensure!(now<old,"provider LOCAL heartbeat cutoff lost");
  if state.pending.is_none(){if old-now>5_000_000_000{return Ok(())}state.pending=Some(ProviderPending{request_id:uuid::Uuid::new_v4().to_string(),expected_serial:state.grant.heartbeat_serial.clone(),operation:None,ack:None,transport:FinalizationBudget::begin(Duration::from_nanos((old-now).min(2_000_000_000)))?,cancelled:self.fenced.clone()});}
  let pending=state.pending.as_mut().unwrap();pending.transport.remaining()?;
  if pending.operation.is_none(){let operation=EngineOperationKind::HeartbeatProviderOwnerV1{workspace_id:self.workspace.clone(),owner_epoch:state.grant.owner_epoch.clone(),ownership_id:state.grant.ownership_id.clone(),request_id:pending.request_id.clone(),expected_serial:pending.expected_serial.clone()};
   if !self.registration().try_begin_owner_renewal_retaining(&mut pending.operation,operation,format!("provider-hb:{}",pending.request_id),pending.transport.clone(),pending.cancelled.clone())?{return Ok(())}}
  if pending.ack.is_none()&&!pending.operation.as_mut().unwrap().take_original_provider_heartbeat_ack_retaining(&mut pending.ack)?{return Ok(())}let ack=pending.ack.as_ref().unwrap();ack.assert_current()?;
  ensure!(Arc::ptr_eq(ack.registration(),self.registration()),"provider heartbeat registration differs");let grant:ProviderGrant=serde_json::from_value(ack.owner_grant().clone()).map_err(|_|fault())?;grant.validate_data(self.registration().data())?;
  ensure!(state.grant.same_owner(&grant)&&grant.request_id==pending.request_id&&Counter::parse(&grant.heartbeat_serial)?.value()==Counter::parse(&pending.expected_serial)?.value().checked_add(1).context("provider serial exhausted")?,"provider heartbeat changed actual owner");
  // A later heartbeat cannot conceal a lost ORIGINAL local cutoff. This is
  // liveness fencing, not a global lease or permission for another owner.
  self.adopting.store(true,Ordering::Release);ensure!(self.sample()?<old,"provider old local cutoff lost before adoption");self.registration().assert_current()?;self.config.assert_current()?;
  let budget=ack.original_liveness_budget().clone();let(deadline,at)=budget.observed_clock_bounds()?;ensure!(deadline>at,"provider heartbeat original local clock lost");
  state.grant=grant;state.budget=budget;state.heartbeat=Some(ack.clone());self.deadline.store(deadline,Ordering::Release);ensure!(self.sample()?<old,"provider old local cutoff lost during adoption");
  state.pending=None;self.adopting.store(false,Ordering::Release);ensure!(self.admit_on_app(),"provider local heartbeat admission closed");Ok(())
 }
 pub(super) fn provider_has_no_pending_data(&self)->bool{self.provider_state.try_lock().ok().is_some_and(|s|s.as_ref().is_some_and(|s|s.pending.is_none()))}
}
