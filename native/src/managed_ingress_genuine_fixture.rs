//! Nondefault, genuine-bootstrap-only boundary fixture. DATA never issues
//! source/runtime/retirement authority. No owner has a serialized constructor.
use super::*;
use crate::managed_machine_ingress::{IngressJob, ManagedIngressControl, FixtureJobData};
use rudder_engine::execution::handoff::WorkerJoined;
use std::sync::{OnceLock, Weak};

#[cfg(not(any(feature="managed-ingress-fixture-close-wins",feature="managed-ingress-fixture-admission-wins",feature="managed-ingress-fixture-carrier-contention")))]
compile_error!("genuine ingress fixture requires exactly one case");
#[cfg(any(all(feature="managed-ingress-fixture-close-wins",feature="managed-ingress-fixture-admission-wins"),all(feature="managed-ingress-fixture-close-wins",feature="managed-ingress-fixture-carrier-contention"),all(feature="managed-ingress-fixture-admission-wins",feature="managed-ingress-fixture-carrier-contention")))]
compile_error!("genuine ingress fixture cases are mutually exclusive");

#[derive(Clone,Copy,PartialEq,Eq)]
enum Case {CloseWins,AdmissionWins,CarrierContention}
const CASE:Case=if cfg!(feature="managed-ingress-fixture-close-wins"){Case::CloseWins}
    else if cfg!(feature="managed-ingress-fixture-admission-wins"){Case::AdmissionWins}else{Case::CarrierContention};
impl Case {fn name(self)->&'static str{match self{Self::CloseWins=>"CloseWins",Self::AdmissionWins=>"AdmissionWins",Self::CarrierContention=>"CarrierContention"}}}
const WAIT:u8=0;const CONTINUE:u8=1;const ABORT:u8=2;

/// Installed in ingress and referenced weakly by the SAME delivered Job.
/// No control alias, strong Job, payload snapshot, JoinHandle or callback.
/// The sole budget/cancel aliases come from that Job's original carrier.
pub(crate) struct FixtureGate {
    job:OnceLock<Weak<IngressJob>>,original:OnceLock<FinalizationBudget>,cancel:OnceLock<Arc<AtomicBool>>,
    insert_seen:AtomicBool,insert_action:AtomicU8,carrier_seen:AtomicBool,carrier_action:AtomicU8,
    post_root_seen:AtomicBool,root_refused:AtomicBool,actual_carrier_would_block:AtomicBool,aborted:AtomicBool,
}
impl FixtureGate {
    fn new()->Self{Self{job:OnceLock::new(),original:OnceLock::new(),cancel:OnceLock::new(),insert_seen:AtomicBool::new(false),insert_action:AtomicU8::new(WAIT),carrier_seen:AtomicBool::new(false),carrier_action:AtomicU8::new(WAIT),post_root_seen:AtomicBool::new(false),root_refused:AtomicBool::new(false),actual_carrier_would_block:AtomicBool::new(false),aborted:AtomicBool::new(false)}}
    pub(crate) fn abort(&self){self.aborted.store(true,Ordering::Release);self.insert_action.store(ABORT,Ordering::Release);self.carrier_action.store(ABORT,Ordering::Release);}
    fn release(&self,action:&AtomicU8)->Result<()>{self.current()?;ensure!(!self.aborted.load(Ordering::Acquire),"fixture original handshake aborted");
        action.compare_exchange(WAIT,CONTINUE,Ordering::AcqRel,Ordering::Acquire).map_err(|_|anyhow::anyhow!("fixture original release occupied/aborted"))?;
        // Abort cannot be changed back to Continue by this CAS.
        ensure!(!self.aborted.load(Ordering::Acquire),"fixture original handshake aborted");Ok(())}
    fn current(&self)->Result<()>{let budget=self.original.get().context("fixture original command not delivered")?;
        let cancel=self.cancel.get().context("fixture original cancellation absent")?;time(budget,cancel)}
    fn wait_flag(&self,flag:&AtomicBool)->Result<()>{loop{self.current()?;ensure!(!self.aborted.load(Ordering::Acquire),"fixture handshake aborted");
        if flag.load(Ordering::Acquire){return Ok(());}std::thread::yield_now();}}
    fn original_job(&self)->Result<Arc<IngressJob>>{self.job.get().and_then(Weak::upgrade).context("fixture original Job absent")}
    pub(crate) fn after_insert(self:&Arc<Self>,job:&Arc<IngressJob>)->Result<()> {
        ensure!(job.fixture_is_open_prepared_project(),"fixture requires original ordinary OpenPreparedProjectV1");
        let (original,cancel)=job.fixture_original_budget_and_cancel()?;
        job.fixture_attach_gate(self)?;
        self.job.set(Arc::downgrade(job)).map_err(|_|anyhow::anyhow!("fixture original Job already captured"))?;
        self.original.set(original).map_err(|_|anyhow::anyhow!("fixture original budget already captured"))?;
        self.cancel.set(cancel).map_err(|_|anyhow::anyhow!("fixture original cancellation already captured"))?;
        self.insert_seen.store(true,Ordering::Release);
        let result=(||{loop{self.current()?;match self.insert_action.load(Ordering::Acquire){
            CONTINUE if !self.aborted.load(Ordering::Acquire)=>return Ok(()),ABORT=>anyhow::bail!("fixture original insertion aborted"),_=>std::thread::yield_now()}}})();
        if result.is_err(){self.abort();}result
    }
    /// Called ONLY while holding the real WorkerCarrier, immediately before
    /// its original reserve_command. Fixed atomics/yield only: no clock, IO,
    /// payload mutex, join, callback or alternative reservation implementation.
    pub(crate) fn before_actual_carrier_charge(&self)->Result<()> {
        if CASE!=Case::CarrierContention{ensure!(!self.aborted.load(Ordering::Acquire),"fixture original charge aborted");return Ok(());}
        self.carrier_seen.store(true,Ordering::Release);
        loop{match self.carrier_action.load(Ordering::Acquire){
            CONTINUE=>{ensure!(!self.aborted.load(Ordering::Acquire),"fixture original charge aborted");return Ok(());},
            ABORT=>anyhow::bail!("fixture original charge aborted before root mutation"),_=>std::thread::yield_now()}}
    }
    pub(crate) fn actual_carrier_would_block(&self){if self.carrier_seen.load(Ordering::Acquire){self.actual_carrier_would_block.store(true,Ordering::Release);}}
    pub(crate) fn root_refused(&self){self.root_refused.store(true,Ordering::Release);}
    /// Deliberately NEVER permits downstream kernel/SDK/receipt/queue effects.
    /// The original candidate remains retained after the fixture coordinator
    /// latches Abort; no success reply or command completion is manufactured.
    pub(crate) fn after_root_pre_effect(&self)->Result<()> {
        self.current()?;self.post_root_seen.store(true,Ordering::Release);
        loop{if self.aborted.load(Ordering::Acquire){anyhow::bail!("fixture pre-effect original candidate retained");}
            if let Err(error)=self.current(){self.abort();return Err(error);}std::thread::yield_now();}
    }
}
struct State {
    machine:MachineControl,ingress:ManagedIngressControl,gate:Arc<FixtureGate>,
    drain:Mutex<Option<CoveredTaskGenerationDrain>>,sample:Mutex<Option<Sample>>,
    coordinator_error:Mutex<Option<anyhow::Error>>,coordinator_panic:Mutex<Option<Box<dyn Any+Send>>>,
    observer_error:Mutex<Option<anyhow::Error>>,observer_panic:Mutex<Option<Box<dyn Any+Send>>>,arm_panic:Mutex<Option<Box<dyn Any+Send>>>,invalid:AtomicBool,
    coordinator_owned:AtomicBool,observer_owned:AtomicBool,
}
struct Sample {job:FixtureJobData,exact_captured_root:bool,closed:bool,actual_contention:bool}
/// Lives in run_selected beside the WHOLE runtime/ingress, through quarantine.
/// State has no join owner: worker -> State cannot cycle back to that worker.
#[must_use="retain the same fixture partial owners and actual joins"]
pub(crate) struct FixtureOwner {
    state:Arc<State>,coordinator:Option<OwnedWorkerJoin>,observer:Option<OwnedWorkerJoin>,
    coordinator_joined:Option<WorkerJoined>,observer_joined:Option<WorkerJoined>,
}
impl FixtureOwner {
    pub(crate) fn arm_retaining(out:&mut Option<Self>,machine:MachineControl,ingress:ManagedIngressControl)->Result<()> {
        ensure!(out.is_none(),"original fixture owner already retained");
        let state=Arc::new(State{machine,ingress,gate:Arc::new(FixtureGate::new()),drain:Mutex::new(None),sample:Mutex::new(None),coordinator_error:Mutex::new(None),coordinator_panic:Mutex::new(None),observer_error:Mutex::new(None),observer_panic:Mutex::new(None),arm_panic:Mutex::new(None),invalid:AtomicBool::new(false),coordinator_owned:AtomicBool::new(false),observer_owned:AtomicBool::new(false)});
        *out=Some(Self{state:state.clone(),coordinator:None,observer:None,coordinator_joined:None,observer_joined:None});
        let owner=out.as_mut().unwrap();
        let armed=catch_unwind(AssertUnwindSafe(||->Result<()>{
            ensure!(state.ingress.belongs_to_machine(&state.machine),"fixture foreign original ingress");
            state.ingress.fixture_attach_gate(&state.gate)?;
            let actual=state.clone();
            let worker=std::thread::Builder::new().name("rudder-ingress-fixture".into()).spawn(move||{
                while !actual.coordinator_owned.load(Ordering::Acquire){std::thread::yield_now();}
                let result=catch_unwind(AssertUnwindSafe(||coordinate(&actual)));
                // Panic/expiry/refusal releases only absorbing Abort. This does
                // not drop owners, settle the original ingress or close ledger.
                actual.gate.abort();
                match result{Ok(Ok(sample))=>{*actual.sample.lock().unwrap_or_else(|p|{actual.invalid.store(true,Ordering::Release);p.into_inner()})=Some(sample);},
                    Ok(Err(error))=>{actual.invalid.store(true,Ordering::Release);*actual.coordinator_error.lock().unwrap_or_else(|p|p.into_inner())=Some(error);},
                    Err(panic)=>{actual.invalid.store(true,Ordering::Release);*actual.coordinator_panic.lock().unwrap_or_else(|p|p.into_inner())=Some(panic);}}
            })?;
            owner.coordinator=Some(OwnedWorkerJoin::new(worker));
            state.coordinator_owned.store(true,Ordering::Release);
            let original=owner.coordinator.as_ref().unwrap().clone();let actual=state.clone();
            let observer=std::thread::Builder::new().name("rudder-ingress-fixture-data".into()).spawn(move||{
                while !actual.observer_owned.load(Ordering::Acquire){std::thread::yield_now();}
                let result=catch_unwind(AssertUnwindSafe(||observe_and_emit(&actual,&original)));
                if !matches!(result,Ok(Ok(()))){actual.invalid.store(true,Ordering::Release);actual.gate.abort();
                    match result{Ok(Err(error))=>*actual.observer_error.lock().unwrap_or_else(|p|p.into_inner())=Some(error),Err(panic)=>*actual.observer_panic.lock().unwrap_or_else(|p|p.into_inner())=Some(panic),_=>{}}}
            })?;
            owner.observer=Some(OwnedWorkerJoin::new(observer));state.observer_owned.store(true,Ordering::Release);Ok(())
        }));
        match armed {
            Ok(result)=>{if result.is_err(){state.invalid.store(true,Ordering::Release);state.gate.abort();}result},
            Err(panic)=>{state.invalid.store(true,Ordering::Release);state.gate.abort();
                *state.arm_panic.lock().unwrap_or_else(|p|p.into_inner())=Some(panic);
                anyhow::bail!("fixture arm panic retained with original partial owners")}
        }
    }
    /// App-pure cached observations only. No spawn, join, clock, IO or wait.
    pub(crate) fn poll_cached_on_app(&mut self)->Result<()> {
        let result=(||->Result<()>{
            if self.coordinator_joined.is_none(){if let Some(actual)=&self.coordinator{self.coordinator_joined=actual.try_joined()?;}}
            if self.observer_joined.is_none(){if let Some(actual)=&self.observer{self.observer_joined=actual.try_joined()?;}}
            Ok(())
        })();
        if result.is_err(){self.state.invalid.store(true,Ordering::Release);self.state.gate.abort();}result
    }
}
fn poll_close(state:&State,begin:bool)->Result<CoveredTaskGenerationProgress>{
    let original=state.gate.original.get().context("fixture original command absent")?.clone();
    let cancelled=state.gate.cancel.get().context("fixture original cancellation absent")?.clone();
    let mut held=state.drain.try_lock().map_err(|_|anyhow::anyhow!("fixture original close output busy/poisoned"))?;
    if begin{state.machine.begin_covered_task_generation_drain_retaining(&mut held,state.ingress.clone(),original,cancelled)}
    else{state.machine.poll_covered_task_generation_drain_retaining(held.as_mut().context("fixture original close campaign absent")?)}
}
fn closed(state:&State)->Result<CoveredTaskGenerationProgress>{loop{state.gate.current()?;let progress=poll_close(state,false)?;
    if progress.covered_ingress_fresh_roots_closed&&progress.original_ingress_roots_associated{return Ok(progress);}std::thread::yield_now();}}
fn coordinate(state:&State)->Result<Sample>{
    while !state.gate.insert_seen.load(Ordering::Acquire){ensure!(!state.gate.aborted.load(Ordering::Acquire),"fixture arm refused before command");std::thread::yield_now();}
    state.gate.current()?;let job=state.gate.original_job()?;
    let progress=match CASE {
        Case::CloseWins=>{poll_close(state,true)?;let progress=closed(state)?;
            state.gate.release(&state.gate.insert_action)?;state.gate.wait_flag(&state.gate.root_refused)?;progress},
        Case::AdmissionWins=>{state.gate.release(&state.gate.insert_action)?;state.gate.wait_flag(&state.gate.post_root_seen)?;
            poll_close(state,true)?;closed(state)?},
        Case::CarrierContention=>{state.gate.release(&state.gate.insert_action)?;state.gate.wait_flag(&state.gate.carrier_seen)?;
            let pending=poll_close(state,true)?;
            ensure!(!pending.covered_ingress_fresh_roots_closed&&!pending.original_boundary_roots_captured,"fixture busy carrier unexpectedly closed");
            ensure!(state.gate.actual_carrier_would_block.load(Ordering::Acquire),"fixture actual WorkerCarrier WouldBlock absent");
            state.gate.release(&state.gate.carrier_action)?;state.gate.wait_flag(&state.gate.post_root_seen)?;closed(state)?},
    };
    let observed=loop{state.gate.current()?;if let Some(data)=state.ingress.fixture_job_data(&job)?{break data;}std::thread::yield_now();};
    let exact=if let Some(root)=job.covered_root(){let held=state.drain.try_lock().map_err(|_|anyhow::anyhow!("fixture original close output busy/poisoned"))?;
        held.as_ref().is_some_and(|drain|drain.fixture_contains_original_command_root(root))}else{false};
    ensure!(observed.original_job_retained&&observed.no_effects&&observed.no_descendants,"fixture original candidate effects/identity invalid");
    if CASE==Case::CloseWins{ensure!(!observed.root_present&&!exact&&observed.root_error_retained,"fixture close-wins original refusal absent");}
    else{ensure!(observed.root_present&&exact&&state.gate.post_root_seen.load(Ordering::Acquire),"fixture original admitted root absent");}
    state.gate.current()?;
    Ok(Sample{job:observed,exact_captured_root:exact,closed:progress.covered_ingress_fresh_roots_closed,actual_contention:state.gate.actual_carrier_would_block.load(Ordering::Acquire)})
}
#[derive(serde::Serialize)]
struct FixtureData {
    schema:u8,case:&'static str,fixture_boundary_observed:bool,current:bool,
    original_job_retained:bool,root_present:bool,exact_original_root_captured:bool,original_root_refusal_retained:bool,
    candidate_pre_effect:bool,no_descendants:bool,actual_worker_carrier_would_block:bool,
    coordinator_joined_including_tls:bool,original_ingress_worker_pending:bool,original_runtime_worker_pending:bool,reporting_worker_pending:bool,
    covered_roots_closed:bool,command_sha256:Option<String>,session_generation_sha256:Option<String>,
    source_accepted:bool,runtime_ready:bool,retirement_complete:bool,
}
fn observe_and_emit(state:&State,coordinator:&OwnedWorkerJoin)->Result<()> {
    // Only this OFF-App worker starts the original join observer. No is_finished
    // or worker-return message can substitute for its actual TLS-complete join.
    coordinator.start_join_observer()?;
    let mut joined=None;
    loop{if let Some(actual)=coordinator.try_joined()?{joined=Some(actual);break;}
        if state.gate.original.get().is_some()&&state.gate.current().is_err(){state.invalid.store(true,Ordering::Release);state.gate.abort();break;}
        if state.gate.aborted.load(Ordering::Acquire)&&state.gate.original.get().is_none(){break;}
        std::thread::yield_now();}
    let current=state.gate.current().is_ok()&&!state.invalid.load(Ordering::Acquire);
    let sample=state.sample.try_lock().map_err(|_|anyhow::anyhow!("fixture retained sample busy/poisoned"))?;
    let s=sample.as_ref();
    let data=FixtureData{schema:1,case:CASE.name(),fixture_boundary_observed:current&&joined.is_some()&&s.is_some(),current,
        original_job_retained:s.is_some_and(|s|s.job.original_job_retained),root_present:s.is_some_and(|s|s.job.root_present),
        exact_original_root_captured:s.is_some_and(|s|s.exact_captured_root),original_root_refusal_retained:s.is_some_and(|s|s.job.root_error_retained),
        candidate_pre_effect:s.is_some_and(|s|s.job.no_effects),no_descendants:s.is_some_and(|s|s.job.no_descendants),
        actual_worker_carrier_would_block:s.is_some_and(|s|s.actual_contention),coordinator_joined_including_tls:joined.is_some(),
        original_ingress_worker_pending:true,original_runtime_worker_pending:true,reporting_worker_pending:true,covered_roots_closed:s.is_some_and(|s|s.closed),
        command_sha256:s.map(|s|s.job.command_sha256.clone()),session_generation_sha256:s.map(|s|s.job.session_generation_sha256.clone()),
        source_accepted:false,runtime_ready:false,retirement_complete:false};
    drop(sample);
    // One bounded closed DATA frame, best effort; no private errors/paths/body.
    if let Ok(raw)=serde_json::to_vec(&data){if raw.len()<=4096{use std::io::Write;
        let mut output=std::io::stderr().lock();let _=output.write_all(b"RUDDER_GENUINE_INGRESS_BOUNDARY_DATA_V1=");let _=output.write_all(&raw);let _=output.write_all(b"\n");}}
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn abort_is_absorbing_for_both_original_release_actions(){let gate=FixtureGate::new();gate.abort();
        for action in [&gate.insert_action,&gate.carrier_action]{assert_eq!(action.load(Ordering::Acquire),ABORT);assert!(action.compare_exchange(WAIT,CONTINUE,Ordering::AcqRel,Ordering::Acquire).is_err());}
        assert!(gate.before_actual_carrier_charge().is_err());}
    #[test]
    fn opaque_gate_does_not_create_command_budget_or_payload(){let gate=FixtureGate::new();assert!(gate.job.get().is_none());assert!(gate.original.get().is_none());assert!(gate.cancel.get().is_none());assert!(gate.current().is_err());}
    #[test]
    fn reporting_requires_actual_coordinator_join_including_held_tls() {
        struct HeldTls {entered:Arc<AtomicBool>,release:Arc<AtomicBool>}
        impl Drop for HeldTls {fn drop(&mut self){self.entered.store(true,Ordering::Release);
            while !self.release.load(Ordering::Acquire){std::thread::yield_now();}}}
        thread_local! {static ORIGINAL_TLS:std::cell::RefCell<Option<HeldTls>>=const {std::cell::RefCell::new(None)};}
        let entered=Arc::new(AtomicBool::new(false));let release=Arc::new(AtomicBool::new(false));
        let worker_entered=entered.clone();let worker_release=release.clone();
        let actual=std::thread::spawn(move||ORIGINAL_TLS.with(|slot|*slot.borrow_mut()=Some(HeldTls{entered:worker_entered,release:worker_release})));
        let owned=OwnedWorkerJoin::new(actual);owned.start_join_observer().unwrap();
        let original=FinalizationBudget::begin(Duration::from_secs(5)).unwrap();
        while !entered.load(Ordering::Acquire){if let Err(error)=original.remaining(){release.store(true,Ordering::Release);panic!("held TLS entry did not settle: {error}");}std::thread::yield_now();}
        let pending=owned.try_joined();release.store(true,Ordering::Release);
        assert!(pending.unwrap().is_none(),"body return never substitutes for held TLS join");
        let joined=loop{original.remaining().unwrap();if let Some(joined)=owned.try_joined().unwrap(){break joined;}std::thread::yield_now();};
        assert!(joined.same_worker(&owned.try_joined().unwrap().unwrap()));
    }
}
