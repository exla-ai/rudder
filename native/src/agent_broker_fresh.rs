//! Actual in-engine fresh producer. No path/JSON/boolean/lease-only constructor.
use super::*;
use crate::{managed_app::ManagedTaskJournalOwner,managed_launch::WorkspaceAssignment,managed_store::ManagedRunLocation,
    fabric_runtime::SharedAgentToolsProjectionOwner};
use rudder_engine::{agent_api::RetainedAgentJournal,journal::{Journal,GuardedAgentJournal,AgentJournalUse,AgentAdmissionError,
    AgentCommitAdmission,AgentAdmissionPoint,AgentAdmissionState},execution::finalization_budget::FinalizationBudget};
use rudder_native::managed_owner_lease::{ManagedOwnerLease,ManagedOwnerAdmission};

struct Admission {project:Arc<ManagedTaskJournalOwner>,location:ManagedRunLocation,projection:SharedAgentToolsProjectionOwner,
    assignment:WorkspaceAssignment,lease:Arc<ManagedOwnerLease>,closed:AtomicBool}
impl AgentCommitAdmission for Admission {
    fn check(&self,_point:AgentAdmissionPoint)->AgentAdmissionState {
        if self.closed.load(Ordering::Acquire){return AgentAdmissionState::Fenced;}
        if let Err(error)=self.project.check_shared_admission(){return if error.is::<crate::engine_host::EngineSharedAdmissionBusy>(){AgentAdmissionState::Busy}else{AgentAdmissionState::Fenced};}
        match self.lease.admission_state(){ManagedOwnerAdmission::Busy=>return AgentAdmissionState::Busy,ManagedOwnerAdmission::Fenced=>return AgentAdmissionState::Fenced,_=>{}}
        self.projection.broker_admission(&self.project,&self.assignment)
    }
}
struct ConnectionSlot {original:Option<Journal>,guarded:Option<GuardedAgentJournal>}
struct OriginalCall {identity:AgentIdentityFile,_operation:Operation,unknown:bool}
impl Drop for OriginalCall {fn drop(&mut self){let mut token=std::mem::take(&mut self.identity.token).into_bytes();token.fill(0);}}
pub(super) struct FreshBrokerOwner {connection:Mutex<ConnectionSlot>,admission:Arc<Admission>,calls:Mutex<BTreeMap<String,OriginalCall>>}
impl FreshBrokerOwner {
    pub(super) fn stage(project:Arc<ManagedTaskJournalOwner>,location:ManagedRunLocation,projection:SharedAgentToolsProjectionOwner,
        assignment:WorkspaceAssignment,journal:&mut Option<Journal>)->Result<Arc<Self>> {
        let lease=project.managed_owner_lease().context("fresh broker requires actual assigned owner lease")?.clone();
        ensure!(journal.is_some(),"actual broker writer absent");
        Ok(Arc::new(Self{connection:Mutex::new(ConnectionSlot{original:journal.take(),guarded:None}),
            admission:Arc::new(Admission{project,location,projection,assignment,lease,closed:AtomicBool::new(false)}),calls:Mutex::new(BTreeMap::new())}))
    }
    fn connection(&self)->Result<std::sync::MutexGuard<'_,ConnectionSlot>> {match self.connection.try_lock(){Ok(slot)=>Ok(slot),Err(std::sync::TryLockError::WouldBlock)=>Err(AgentAdmissionError::Busy.into()),Err(_)=>{self.admission.closed.store(true,Ordering::Release);Err(AgentAdmissionError::EffectUnknown.into())}}}
    pub(super) fn prepare(&self,config:&BrokerConfig,original:&FinalizationBudget,cancel:&AtomicBool)->Result<()> {
        original.remaining()?;ensure!(!cancel.load(Ordering::Acquire),"fresh broker setup cancelled");
        self.admit(false)?;let a=&self.admission;
        ensure!(a.project.owner()==&a.assignment.fence.owner&&a.location.run_id()==a.assignment.fence.run_id
            &&a.location.store().binding().project_id==a.assignment.fence.owner.project_id
            &&config.journal_path==a.location.store().binding().journal
            &&a.projection.assignment()==&a.assignment
            &&a.lease.project_id()?==a.assignment.fence.owner.project_id
            &&Arc::ptr_eq(a.project.managed_owner_lease().context("actual fresh lease absent")?,&a.lease),"fresh broker original assignment/source/lease differs");
        ensure!(config.socket_path==socket_path(&a.location.store().binding().home,&a.assignment.fence.owner.project_id,&a.assignment.fence.run_id,&a.assignment.fence.attempt_id),"fresh broker projection endpoint differs");
        let mut slot=self.connection()?;
        let journal=slot.original.as_ref().context("original fresh broker writer already transferred")?;
        a.project.check_broker_writer(journal)?;a.assignment.fence.check_current(journal)?;
        ensure!(a.location.store().reopen_current(journal)?==*a.location.store()
            &&a.location.assignment(&a.assignment.fence.attempt_id)?==Some(a.assignment.clone()),"fresh broker original namespace/assignment changed");
        original.remaining()?;self.admit(false)?;
        let ConnectionSlot{original:journal,guarded}=&mut *slot;
        GuardedAgentJournal::stage_retaining(guarded,journal,a.clone())?;
        guarded.as_mut().unwrap().install()?;original.remaining()?;self.admit(false)
    }
    pub(super) fn api(self:&Arc<Self>,identity:&Path)->Result<AgentApi>{AgentApi::from_retained(identity,self.clone())}
    pub(super) fn fence(&self){self.admission.closed.store(true,Ordering::Release);}
    pub(super) fn admit(&self,observation:bool)->Result<()> {
        if observation {self.admission.project.assert_original_held_custody_off_app()?;return Ok(());}
        match self.admission.check(AgentAdmissionPoint::Dispatch){AgentAdmissionState::Ready=>Ok(()),AgentAdmissionState::Busy=>Err(AgentAdmissionError::Busy.into()),AgentAdmissionState::Fenced=>Err(AgentAdmissionError::Fenced.into())}
    }
    pub(super) fn observation(operation:&Operation)->bool {
        matches!(operation,Operation::Cancel{..})||matches!(operation,Operation::Call{tool,..} if tool=="agents_status")
            ||matches!(operation,Operation::Call{tool,arguments} if tool=="agents_await"&&arguments.get("command_id").is_some_and(Value::is_string))
    }
    pub(super) fn reserve(&self,request:&Request)->Result<()> {
        if Self::observation(&request.operation){return Ok(());}
        let mut calls=self.calls.try_lock().map_err(|_|AgentAdmissionError::Busy)?;
        if calls.contains_key(&request.id)||calls.len()>=MAX_CALLS{return Err(AgentAdmissionError::Busy.into());}
        calls.insert(request.id.clone(),OriginalCall{identity:request.identity.clone(),_operation:request.operation.clone(),unknown:false});Ok(())
    }
    pub(super) fn finish(&self,id:&str,result:&Result<Value>){
        let mut calls=self.calls.lock().unwrap_or_else(|p|p.into_inner());
        if result.as_ref().err().is_some_and(|e|e.downcast_ref::<AgentAdmissionError>()==Some(&AgentAdmissionError::EffectUnknown)){
            if let Some(call)=calls.get_mut(id){call.unknown=true;}
        }else{calls.remove(id);}
    }
    pub(super) fn mark_pending_unknown(&self,id:&str){if let Some(call)=self.calls.lock().unwrap_or_else(|p|p.into_inner()).get_mut(id){call.unknown=true;}}
    pub(super) fn spawn_failed(&self,id:&str){self.calls.lock().unwrap_or_else(|p|p.into_inner()).remove(id);}
    pub(super) fn recovery_data(&self)->Result<Value>{
        let calls=self.calls.try_lock().map_err(|_|AgentAdmissionError::Busy)?;
        let connection_unknown=self.connection.try_lock().map_or(true,|slot|slot.guarded.as_ref().is_some_and(GuardedAgentJournal::has_unknown));
        Ok(json!({"scope":"retained-fresh-broker-recovery-data-only","connection_effect_unknown":connection_unknown,"calls":calls.iter().map(|(id,call)|json!({"request_id":id,"tool":match &call._operation{Operation::Call{tool,..}=>match tool.as_str(){"agents_find"|"agents_send"|"agents_ask"|"agents_reply"|"agents_inbox"|"agents_ack"|"agents_status"|"agents_topics"|"agents_steer"|"agents_interrupt"|"agents_resume"|"agents_await"=>tool.as_str(),_=>"unrecognized"},Operation::Pending=>"pending",_=>"internal"},"state":if call.unknown{"effect_unknown"}else{"pending_original_call"}})).collect::<Vec<_>>(),"normal_retirement":false,"replay_authorized":false}))
    }
    pub(super) fn has_unsettled(&self)->bool {
        if self.calls.try_lock().map_or(true,|c|!c.is_empty()){return true;}
        self.connection.try_lock().map_or(true,|slot|slot.guarded.as_ref().is_some_and(GuardedAgentJournal::has_unknown))
    }
}
impl RetainedAgentJournal for FreshBrokerOwner {
    fn with_journal(&self,usage:AgentJournalUse,cancel:Arc<AtomicBool>,work:&mut dyn FnMut(&mut Journal)->Result<()>)->Result<()> {
        self.admission.project.assert_original_held_custody_off_app()?;
        self.admit(usage==AgentJournalUse::RetainedObservation)?;
        let mut slot=self.connection()?;
        let journal=slot.guarded.as_mut().context("fresh broker guarded writer not installed")?;
        let result=journal.run(usage,cancel,|journal|{self.admission.project.check_broker_writer(journal)?;work(journal)});
        if self.admission.project.assert_original_held_custody_off_app().is_err(){return if usage==AgentJournalUse::SharedTool{journal.post_admission(AgentAdmissionState::Fenced)}else{Err(AgentAdmissionError::Fenced.into())};}
        if result.is_ok()&&usage==AgentJournalUse::SharedTool {journal.post_admission(self.admission.check(AgentAdmissionPoint::Dispatch))?;}
        result
    }
}
