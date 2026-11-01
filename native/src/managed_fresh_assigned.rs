//! Genuine ACTIVE lease + original local genesis adoption. Backend selection,
//! App/task admission and Root seed reader enrollment still occur explicitly
//! through existing callers AFTER this retained owner is known current.
#![cfg(target_os="linux")]
use anyhow::{ensure,Context,Result};
use crate::{engine_host::EngineOwnership,managed_fresh_genesis::ManagedFreshGenesis};
use rudder_native::managed_owner_lease::{ManagedOwnerLease,ManagedOwnerWatchdog,ManagedOwnerAdmission};
use rudder_engine::execution::{finalization_budget::FinalizationBudget,handoff::WorkerJoined};
use std::sync::{Arc,atomic::{AtomicBool,Ordering}};
/// Only real lease+source capsules can construct this value. Engine lifetime
/// retains it through all source/task/cache/acceptance aliases and cleanup.
pub(crate) struct AssignedFreshAuthority {source:Arc<ManagedFreshGenesis>,lease:Arc<ManagedOwnerLease>,known:AtomicBool,fenced:AtomicBool}
impl AssignedFreshAuthority {
    pub(crate) fn admission_state(&self)->ManagedOwnerAdmission{if !self.known.load(Ordering::Acquire)||self.fenced.load(Ordering::Acquire)||self.source.check_machine_admission().is_err(){ManagedOwnerAdmission::Fenced}else{self.lease.admission_state()}}
    pub(crate) fn check_active(&self)->Result<()> {self.source.check_machine_admission()?;ensure!(self.known.load(Ordering::Acquire)&&!self.fenced.load(Ordering::Acquire),"fresh assigned lifetime unacknowledged/fenced");
        self.lease.check_active_for_admission().map_err(|error|if self.lease.admission_state()==ManagedOwnerAdmission::Fenced{error}else{crate::engine_host::EngineSharedAdmissionBusy.into()})}
    pub(crate) fn assert_custody(&self)->Result<()> {self.check_active()?;self.source.assert_assigned_retention()?;
        self.lease.assert_current().map_err(|error|if self.lease.admission_state()==ManagedOwnerAdmission::Fenced{error}else{crate::engine_host::EngineSharedAdmissionBusy.into()})?;self.check_active()}
    pub(crate) fn assert_retained_custody(&self)->Result<()> {ensure!(self.known.load(Ordering::Acquire),"fresh assigned original ACK absent");self.source.assert_assigned_retention()}
    pub(crate) fn lease(&self)->&Arc<ManagedOwnerLease>{&self.lease}
    pub(crate) fn source(&self)->&Arc<ManagedFreshGenesis>{&self.source}
}
/// Keep outside worker catch/delivery. Unknown local transfer/record ACK keeps
/// the original Journal, guard, lease/watchdog and source resources retained.
pub(crate) struct AssignedFreshProject {authority:Arc<AssignedFreshAuthority>,watchdog:Option<ManagedOwnerWatchdog>,watchdog_joined:Option<WorkerJoined>,engine:Option<EngineOwnership>,attempted:bool,acknowledged:bool,dispatched:bool}
impl AssignedFreshProject {
    pub(crate) fn capture(source:Arc<ManagedFreshGenesis>,lease:Arc<ManagedOwnerLease>)->Self {Self{authority:Arc::new(AssignedFreshAuthority{source,lease,known:AtomicBool::new(false),fenced:AtomicBool::new(false)}),watchdog:None,watchdog_joined:None,engine:None,attempted:false,acknowledged:false,dispatched:false}}
    pub(crate) fn adopt_retaining(&mut self,original:&FinalizationBudget,cancel:&AtomicBool)->Result<()> {
        ensure!(!self.attempted,"original assigned adoption already attempted");self.attempted=true;
        let result=(||->Result<()>{time(original,cancel)?;let prep=self.authority.source.preparation();wait_original_lease(&self.authority.lease,original,cancel)?;
            ensure!(Arc::ptr_eq(self.authority.lease.registration(),prep.registration())&&self.authority.lease.project_id()?==prep.data().project_id&&self.authority.lease.workspace_id()==prep.data().workspace_id&&self.authority.lease.owner_instance()==prep.registration().engine_id(),"ACTIVE original lease differs from actual genesis/preparation");
            ManagedOwnerWatchdog::start_retaining(&mut self.watchdog,self.authority.lease.clone())?;
            self.authority.source.promote_engine_retaining(&mut self.engine,self.authority.clone(),original,cancel)?;
            self.authority.known.store(true,Ordering::Release);self.engine.as_ref().context("original assigned Engine absent")?.assert_current_bounded(original,cancel)?;time(original,cancel)?;self.acknowledged=true;Ok(())})();
        if result.is_err(){self.close_admission();}result
    }
    pub(crate) fn assert_current(&self)->Result<()> {ensure!(self.acknowledged&&!self.dispatched,"actual fresh Engine pending/recovery/already dispatched");self.authority.assert_custody()?;self.engine.as_ref().context("actual fresh Engine absent")?.assert_current()}
    pub(crate) fn assert_current_bounded(&self,original:&FinalizationBudget,cancel:&AtomicBool)->Result<()> {loop{time(original,cancel)?;match self.assert_current(){Ok(())=>return time(original,cancel),Err(error) if error.downcast_ref::<crate::engine_host::EngineSharedAdmissionBusy>().is_some()=>std::thread::sleep(original.phase_limit(std::time::Duration::from_millis(2))?),Err(error)=>return Err(error)}}}
    pub(crate) fn source(&self)->&Arc<ManagedFreshGenesis>{self.authority.source()}
    pub(crate) fn take_engine_retaining(&mut self,out:&mut Option<EngineOwnership>,original:&FinalizationBudget,cancel:&AtomicBool)->Result<()> {
        time(original,cancel)?;ensure!(out.is_none(),"actual fresh Engine transfer destination occupied");self.assert_current_bounded(original,cancel)?;self.dispatched=true;*out=self.engine.take();time(original,cancel)
    }
    /// Pure admission fence only. Explicit machine teardown must first stop new
    /// ingress and App launches, retain actual PTYs/helper reaps/actors/writers,
    /// then join this watchdog OFF App. This does not claim task/seed drainage.
    pub(crate) fn close_admission(&self){self.authority.fenced.store(true,Ordering::Release);if let Some(w)=&self.watchdog{w.close_admission();}}
    /// OFF App, under the caller's original drain budget. Store genuine TLS
    /// join BEFORE final fallible checks; timeout/error keeps the same watchdog.
    /// This proves no task/helper/PTY/actor/seed reader or whole-machine drain.
    pub(crate) fn await_watchdog_closed_off_app(&mut self,original:&FinalizationBudget,cancel:&AtomicBool)->Result<()> {
        time(original,cancel)?;self.close_admission();if self.watchdog_joined.is_none(){self.watchdog_joined=Some(self.watchdog.as_mut().context("original watchdog absent")?.await_closed_off_app(original.remaining()?)?);}time(original,cancel)
    }
}
fn time(original:&FinalizationBudget,cancel:&AtomicBool)->Result<()> {if cancel.load(Ordering::Acquire){original.cancel();anyhow::bail!("original fresh owner adoption cancelled");}original.remaining()?;Ok(())}

pub(crate) fn wait_original_lease(lease:&ManagedOwnerLease,original:&FinalizationBudget,cancel:&AtomicBool)->Result<()> {loop{time(original,cancel)?;match lease.assert_current(){Ok(())=>return time(original,cancel),Err(error) if lease.admission_state()!=ManagedOwnerAdmission::Fenced=>std::thread::sleep(original.phase_limit(std::time::Duration::from_millis(2))?),Err(error)=>return Err(error)}}}
