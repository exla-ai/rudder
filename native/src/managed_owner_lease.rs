//! Original Root ACK lease timing, never source/PTY/helper-drain authority.
//! No JSON constructor, restored timestamp, random engine ID or ACK-time anchor.
#![cfg(target_os="linux")]
use anyhow::{ensure,Context,Result};
use serde::Deserialize;
use std::{sync::{Arc,Mutex,OnceLock,atomic::{AtomicBool,AtomicU64,AtomicPtr,Ordering}},time::Duration};
use crate::{engine_registration::{OriginalFreshAcquireAck,OriginalOwnerRenewalAck,RegisteredEngineOwner,EngineOperation,EngineOperationKind},host_ingress::{HostIngressConfigOwner,HostOwnerLeaseSafety}};
use rudder_engine::{execution::{finalization_budget::FinalizationBudget,handoff::{OwnedWorkerJoin,WorkerJoined}},fleet_lease::{Counter,HostClock,SuspendAwareClock}};

fn fault()->anyhow::Error{anyhow::anyhow!("managed owner lease fenced; original owners retained")}
fn uuid(s:&str)->Result<()> {ensure!(uuid::Uuid::parse_str(s).is_ok_and(|v|v.to_string()==s),"owner identity invalid");Ok(())}
#[derive(Clone,Deserialize)]#[serde(rename_all="camelCase",deny_unknown_fields)]
struct Grant {format_version:u32,authority_id:String,account_id:String,project_id:String,engine_instance_id:String,machine_boot_id:String,owner_epoch:String,lease_id:String,renewal_serial:String,request_id:String,issued_at:String,lease_until:String,duration_ms:u32}
impl Grant {fn validate(&self,registered:&RegisteredEngineOwner)->Result<()> {for s in [&self.authority_id,&self.project_id,&self.engine_instance_id,&self.lease_id]{uuid(s)?;}
    ensure!(self.format_version==1&&self.duration_ms==15000&&self.account_id==registered.account_id()&&self.engine_instance_id==registered.engine_id()&&self.machine_boot_id==registered.boot_id(),"original owner binding differs");
    ensure!(Counter::parse(&self.owner_epoch)?.value()>0,"owner epoch invalid");Counter::parse(&self.renewal_serial)?;
    for s in [&self.request_id,&self.issued_at,&self.lease_until]{ensure!(!s.is_empty()&&s.len()<=180,"owner audit field invalid");}Ok(())}
    fn same_owner(&self,other:&Self)->bool {self.authority_id==other.authority_id&&self.account_id==other.account_id&&self.project_id==other.project_id&&self.engine_instance_id==other.engine_instance_id&&self.machine_boot_id==other.machine_boot_id&&self.owner_epoch==other.owner_epoch&&self.lease_id==other.lease_id}}
#[derive(Deserialize)]#[serde(rename_all="camelCase",deny_unknown_fields)]
struct Accepted {format_version:u32,authority_id:String,project_id:String,operation_id:String,sequence:String,kind:String,data:Grant}
struct Pending {request_id:String,expected_serial:String,operation:Option<EngineOperation>,ack:Option<Arc<OriginalOwnerRenewalAck>>,cancelled:Arc<AtomicBool>,transport:FinalizationBudget}
struct IdentityData {project_id:String,owner_epoch:String}
struct State {grant:Grant,budget:FinalizationBudget,renewal_proof:Option<Arc<OriginalOwnerRenewalAck>>,pending:Option<Pending>}

#[derive(Clone,Copy,Debug,PartialEq,Eq)]
pub enum ManagedOwnerAdmission {Ready,Busy,Fenced}
#[derive(Debug)]struct OwnerAdmissionBusy;
impl std::fmt::Display for OwnerAdmissionBusy {fn fmt(&self,f:&mut std::fmt::Formatter<'_>)->std::fmt::Result{f.write_str("managed owner renewal admission busy")}}
impl std::error::Error for OwnerAdmissionBusy {}
/// Shared original ownership clock. Source/Journal/project permission is held
/// separately by App's actual assigned capsule; this type cannot construct it.
pub struct ManagedOwnerLease {
    initial:Option<Arc<OriginalFreshAcquireAck>>,provider_initial:Option<Arc<crate::engine_registration::OriginalProviderAcquireAck>>,provider_state:Mutex<Option<provider::ProviderState>>,config:Arc<HostIngressConfigOwner>,safety:Option<HostOwnerLeaseSafety>,workspace:String,
    identity:OnceLock<IdentityData>,state:Mutex<Option<State>>,clock:Option<HostClock>,pid:u32,last:AtomicU64,deadline:AtomicU64,
    serving:AtomicBool,adopting:AtomicBool,fenced:Arc<AtomicBool>,watchdog_started:AtomicBool,
}
impl ManagedOwnerLease {
    /// Stage the actual ACK/config carriers before validation; failure leaves
    /// them in out and does not unpin/dispose any source or cleanup owner.
    pub fn capture_retaining(out:&mut Option<Arc<Self>>,initial:Arc<OriginalFreshAcquireAck>,config:Arc<HostIngressConfigOwner>)->Result<()> {
        ensure!(out.is_none(),"owner output occupied");
        let workspace=initial.receipt_data().operation.workspace_id().to_owned();let safety=config.data().owner_lease_safety.clone();
        *out=Some(Arc::new(Self {initial:Some(initial),provider_initial:None,provider_state:Mutex::new(None),config,safety,workspace,identity:OnceLock::new(),state:Mutex::new(None),clock:None,pid:std::process::id(),last:AtomicU64::new(0),deadline:AtomicU64::new(0),serving:AtomicBool::new(false),adopting:AtomicBool::new(true),fenced:Arc::new(AtomicBool::new(false)),watchdog_started:AtomicBool::new(false)}));
        let clock=match HostClock::new(){Ok(clock)=>clock,Err(_)=>{out.as_ref().unwrap().fence();return Err(fault());}};Arc::get_mut(out.as_mut().unwrap()).context("owner staging unexpectedly shared")?.clock=Some(clock);
        let held=out.as_ref().unwrap();let result=held.capture_inner();if result.is_err(){held.fence();}result
    }
    fn capture_inner(&self)->Result<()> {ensure!(self.config.data().owner_mode.is_none(),"clock lease cannot adopt provider mode");let initial=self.initial.as_ref().context("original clock lease ACK absent")?;initial.assert_current()?;self.config.assert_current()?;ensure!(self.config.config_sha256()==self.registration().data().binding.config_sha256,"owner original config differs");let safety=self.safety.as_ref().context("qualified owner clock policy unconfigured")?;let reserve=safety.reserve_nanoseconds()?;
        let b=serde_json::to_vec(self.initial.as_ref().unwrap().owner_receipt())?;ensure!(b.len()<=8192,"owner receipt exceeds bound");let accepted:Accepted=serde_json::from_slice(&b).map_err(|_|fault())?;
        let grant=accepted.data;grant.validate(self.registration())?;uuid(&accepted.operation_id)?;ensure!(Counter::parse(&accepted.sequence)?.value()>0,"owner sequence invalid");
        let EngineOperationKind::AcquireFreshOwnerV1 {acquire_key,expected_owner_epoch,..}=&self.initial.as_ref().unwrap().receipt_data().operation else{return Err(fault());};
        let project=self.config.data().projects.iter().find(|p|p.alias==self.workspace).context("owner configured project absent")?;
        ensure!(accepted.format_version==1&&accepted.kind=="owner-acquire"&&accepted.authority_id==grant.authority_id&&accepted.project_id==grant.project_id&&grant.project_id==project.prepare.namespace.project_id&&grant.request_id==*acquire_key&&grant.renewal_serial=="0"&&Counter::parse(&grant.owner_epoch)?.value()==Counter::parse(expected_owner_epoch)?.value()+1,"original acquire differs");
        let budget=self.initial.as_ref().unwrap().original_lease_budget().clone();let (raw,now)=budget.observed_clock_bounds()?;let deadline=raw.checked_sub(reserve).context("unsafe owner duration")?;ensure!(deadline>now,"original owner deadline lost");
        self.identity.set(IdentityData {project_id:grant.project_id.clone(),owner_epoch:grant.owner_epoch.clone()}).map_err(|_|fault())?;
        *self.state.lock().map_err(|_|fault())?=Some(State {grant,budget,renewal_proof:None,pending:None});self.last.store(now,Ordering::Release);self.deadline.store(deadline,Ordering::Release);
        self.initial.as_ref().unwrap().assert_current()?;self.config.assert_current()?;ensure!(self.sample()?<deadline,"original owner expired during adoption");self.serving.store(true,Ordering::Release);self.adopting.store(false,Ordering::Release);Ok(())
    }
    fn sample(&self)->Result<u64>{ensure!(!self.fenced.load(Ordering::Acquire)&&std::process::id()==self.pid,"owner process/fence differs");let prior=self.last.load(Ordering::Acquire);let now=u64::try_from(self.clock.as_ref().context("owner clock unavailable")?.read().map_err(|_|fault())?.nanoseconds()).map_err(|_|fault())?;
        ensure!(now>=prior,"owner clock regressed");self.last.fetch_max(now,Ordering::AcqRel);Ok(now)}
    pub fn fence(&self){self.fenced.store(true,Ordering::Release);self.serving.store(false,Ordering::Release);}
    /// Immediate observation only. No mutex, filesystem, RPC, wait, task-state
    /// mutation or transferable permission. Workers recheck actual custody.
    pub fn admission_state(&self)->ManagedOwnerAdmission {
        if self.fenced.load(Ordering::Acquire){return ManagedOwnerAdmission::Fenced;}
        if !self.serving.load(Ordering::Acquire)||self.adopting.load(Ordering::Acquire){return ManagedOwnerAdmission::Busy;}
        let ok=self.sample().is_ok_and(|n|n<self.deadline.load(Ordering::Acquire));if !ok{self.fence();return ManagedOwnerAdmission::Fenced;}
        if self.fenced.load(Ordering::Acquire){ManagedOwnerAdmission::Fenced}else if self.adopting.load(Ordering::Acquire){ManagedOwnerAdmission::Busy}else{ManagedOwnerAdmission::Ready}
    }
    pub fn admit_on_app(&self)->bool {self.admission_state()==ManagedOwnerAdmission::Ready}
    pub fn assert_current_off_app(&self)->Result<()> {let result=(||{match self.admission_state(){ManagedOwnerAdmission::Busy=>return Err(OwnerAdmissionBusy.into()),ManagedOwnerAdmission::Fenced=>return Err(fault()),ManagedOwnerAdmission::Ready=>{}}
        self.registration().assert_current()?;self.config.assert_current()?;
        if self.provider_initial.is_some(){self.provider_current_off_app()?;}else{let state=self.state.lock().map_err(|_|fault())?;state.as_ref().context("owner state absent")?.budget.remaining()?;}
        match self.admission_state(){ManagedOwnerAdmission::Ready=>Ok(()),ManagedOwnerAdmission::Busy=>Err(OwnerAdmissionBusy.into()),ManagedOwnerAdmission::Fenced=>Err(fault())}
    })();if result.as_ref().err().is_some_and(|e|e.downcast_ref::<OwnerAdmissionBusy>().is_none()){self.fence();}result}
    pub fn check_active_for_admission(&self)->Result<()> {match self.admission_state(){ManagedOwnerAdmission::Ready=>Ok(()),ManagedOwnerAdmission::Busy=>Err(OwnerAdmissionBusy.into()),ManagedOwnerAdmission::Fenced=>Err(fault())}}
    pub fn assert_current(&self)->Result<()> {self.assert_current_off_app()}
    pub fn registration(&self)->&Arc<RegisteredEngineOwner>{if let Some(initial)=&self.initial{initial.registration()}else{self.provider_initial.as_ref().expect("staged provider owner original ACK").registration()}}
    pub fn project_id(&self)->Result<&str>{Ok(&self.identity.get().context("owner identity absent")?.project_id)}
    pub fn workspace_id(&self)->&str{&self.workspace}
    pub fn owner_epoch(&self)->Result<&str>{Ok(&self.identity.get().context("owner identity absent")?.owner_epoch)}
    pub fn owner_instance(&self)->&str{self.registration().engine_id()}
    /// Off-App only. Retains every original pending operation/proof on error;
    /// no retry mints another request ID or replaces a first-send clock.
    pub fn poll_renewal_off_app(&self)->Result<()> {let result=if self.provider_initial.is_some(){self.poll_provider_heartbeat_off_app()}else{self.poll_inner()};if result.as_ref().err().is_some_and(|e|e.downcast_ref::<OwnerAdmissionBusy>().is_some()){return Ok(());}if result.is_err(){self.fence();}result}
    fn poll_inner(&self)->Result<()> {self.assert_current_off_app()?;let mut lock=self.state.lock().map_err(|_|fault())?;let state=lock.as_mut().context("owner state absent")?;
        let now=self.sample()?;let deadline=self.deadline.load(Ordering::Acquire);ensure!(now<deadline,"owner expired before renewal");
        if state.pending.is_none(){if deadline-now>5_000_000_000{return Ok(());}let duration=Duration::from_nanos((deadline-now).min(2_000_000_000));
            state.pending=Some(Pending {request_id:uuid::Uuid::new_v4().to_string(),expected_serial:state.grant.renewal_serial.clone(),operation:None,ack:None,cancelled:Arc::clone(&self.fenced),transport:FinalizationBudget::begin(duration)?});}
        let pending=state.pending.as_mut().unwrap();pending.transport.remaining()?;
        if pending.operation.is_none(){let op=EngineOperationKind::RenewFreshOwnerV1 {workspace_id:self.workspace.clone(),owner_epoch:state.grant.owner_epoch.clone(),lease_id:state.grant.lease_id.clone(),request_id:pending.request_id.clone(),expected_serial:pending.expected_serial.clone()};
            if !self.registration().try_begin_owner_renewal_retaining(&mut pending.operation,op,format!("owner-renew:{}",pending.request_id),pending.transport.clone(),Arc::clone(&pending.cancelled))?{return Ok(());}}
        if pending.ack.is_none()&&!pending.operation.as_mut().unwrap().take_original_owner_renewal_ack_retaining(&mut pending.ack)?{return Ok(());}let ack=pending.ack.as_ref().unwrap();ack.assert_current()?;
        ensure!(Arc::ptr_eq(ack.registration(),self.registration()),"renewal registration differs");let grant:Grant=serde_json::from_value(ack.owner_grant().clone()).map_err(|_|fault())?;grant.validate(self.registration())?;
        ensure!(state.grant.same_owner(&grant)&&grant.request_id==pending.request_id&&Counter::parse(&grant.renewal_serial)?.value()==Counter::parse(&pending.expected_serial)?.value().checked_add(1).context("owner serial exhausted")?,"renewal owner/serial differs");
        // Close App admission around replacement; check the OLD exact cutoff
        // before AND after, so publishing a later clock never hides expiry.
        self.adopting.store(true,Ordering::Release);let old=self.deadline.load(Ordering::Acquire);ensure!(self.sample()?<old,"old owner expired before ACK adoption");
        self.registration().assert_current()?;self.config.assert_current()?;let budget=ack.original_lease_budget().clone();let (raw,at)=budget.observed_clock_bounds()?;let next=raw.checked_sub(self.safety.as_ref().unwrap().reserve_nanoseconds()?).context("renewal clock unsafe")?;ensure!(next>at,"renewal original deadline lost");
        state.grant=grant;state.budget=budget;state.renewal_proof=Some(Arc::clone(ack));self.deadline.store(next,Ordering::Release);ensure!(self.sample()?<old,"old deadline crossed during renewal adoption");
        state.pending=None;self.adopting.store(false,Ordering::Release);ensure!(self.admit_on_app(),"renewed lease unavailable");Ok(())
    }
}

/// Actual original thread join owner, retained off App through TLS settlement.
/// Closing fences admission; it never says PTY/helper/backend/process stopped.
static WATCHDOG_COUNT:AtomicU64=AtomicU64::new(0);
struct RetainedWatchdog {lease:Arc<ManagedOwnerLease>,stop:Arc<AtomicBool>,join:Option<OwnedWorkerJoin>}
static WATCHDOG_RECOVERY:[AtomicPtr<RetainedWatchdog>;256]=[const {AtomicPtr::new(std::ptr::null_mut())};256];
pub struct ManagedOwnerWatchdog {lease:Arc<ManagedOwnerLease>,stop:Arc<AtomicBool>,join:Option<OwnedWorkerJoin>,joined:bool,reserved:bool}
impl ManagedOwnerWatchdog {
    pub fn start_retaining(out:&mut Option<Self>,lease:Arc<ManagedOwnerLease>)->Result<()> {ensure!(out.is_none(),"watchdog output occupied");ensure!(!lease.watchdog_started.swap(true,Ordering::AcqRel),"watchdog already started");if WATCHDOG_COUNT.fetch_update(Ordering::AcqRel,Ordering::Acquire,|n|(n<256).then_some(n+1)).is_err(){lease.fence();return Err(fault());}
        let stop=Arc::new(AtomicBool::new(false));*out=Some(Self {lease:Arc::clone(&lease),stop:Arc::clone(&stop),join:None,joined:false,reserved:true});out.as_ref().unwrap().lease.assert_current_off_app()?;let stored=Arc::new(AtomicBool::new(false));let worker_stored=Arc::clone(&stored);
        let result=std::thread::Builder::new().name("managed-owner-renewal".into()).spawn(move||{while !worker_stored.load(Ordering::Acquire){std::thread::yield_now();}
            let outcome=std::panic::catch_unwind(std::panic::AssertUnwindSafe(||{while !stop.load(Ordering::Acquire){if lease.poll_renewal_off_app().is_err(){break;}std::thread::park_timeout(Duration::from_millis(5));}lease.fence();}));
            if let Err(panic)=outcome{lease.fence();std::panic::resume_unwind(panic);}});
        match result {Ok(thread)=>{out.as_mut().unwrap().join=Some(OwnedWorkerJoin::new(thread));stored.store(true,Ordering::Release);if out.as_ref().unwrap().join.as_ref().unwrap().start_join_observer().is_err(){out.as_ref().unwrap().close_admission();return Err(fault());}Ok(())},Err(_)=>{out.as_ref().unwrap().lease.fence();Err(fault())}}
    }
    pub fn close_admission(&self){self.lease.fence();self.stop.store(true,Ordering::Release);}
    pub fn await_closed_off_app(&mut self,maximum:Duration)->Result<WorkerJoined>{self.close_admission();let proof=self.join.as_ref().context("watchdog original thread absent")?.await_joined(maximum)?;self.joined=true;Ok(proof)}
}

impl Drop for ManagedOwnerWatchdog {fn drop(&mut self){self.close_admission();if !self.reserved{return;}
    let no_pending=if self.lease.provider_initial.is_some(){self.lease.provider_has_no_pending_data()}else{self.lease.state.try_lock().ok().is_some_and(|state|state.as_ref().is_some_and(|s|s.pending.is_none()))};
    if self.joined&&no_pending {WATCHDOG_COUNT.fetch_sub(1,Ordering::AcqRel);return;}
    // Nonblocking close only. Keep actual join and every pending original RPC
    // owner in bounded process recovery; Drop never joins/detaches/claims drain.
    let owner=RetainedWatchdog {lease:Arc::clone(&self.lease),stop:Arc::clone(&self.stop),join:self.join.take()};
    let original=Box::into_raw(Box::new(owner));
    for slot in &WATCHDOG_RECOVERY {if slot.compare_exchange(std::ptr::null_mut(),original,Ordering::AcqRel,Ordering::Acquire).is_ok(){return;}}
    // The reserved256 total bound makes this unreachable for valid owners.
    // Even a corrupted counter cannot drop/close the actual unknown carrier.
    let _retained_until_process_teardown=original;
}}

#[cfg(test)]
#[path="managed_owner_lease_tests.rs"]
mod tests;

#[path="managed_provider_owner.rs"]mod provider;
