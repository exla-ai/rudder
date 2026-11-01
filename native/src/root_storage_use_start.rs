//! Linear permits required at original scoped dispatch/fork/helper/enrollment
//! boundaries. A seal consumes actual unspent permits; no absence/path/flag is
//! promoted to proof. Issuance remains bound to the actual seed-use ledger.
use super::*;
use rudder_engine::{execution::handoff::{OwnedWorkerJoin,WorkerJoined},protocol::ObjectRef};
struct Permit { _origin:Arc<Origin> }
struct Origin { seed:ObjectRef,nonce:uuid::Uuid }
struct Permits {dispatch:Option<Permit>,fork:Option<Permit>,helper:Option<Permit>,enrollment:Option<Permit>}
struct Inner {origin:Arc<Origin>,state:Mutex<StartState>}
struct StartState {open:Permits,claimed:Permits,factory:Option<OwnedWorkerJoin>,cleanup:Option<FinalizationBudget>,seal:Option<Arc<SealInner>>}
#[derive(Clone)]
pub struct SeedTaskUseGate {inner:Arc<Inner>}
struct SealInner {gate:std::sync::Weak<Inner>,origin:Arc<Origin>,_factory:WorkerJoined,
    _helper:Permit,_enrollment:Permit,dispatch:Option<Permit>,fork:Option<Permit>}
/// Admission quiescence ONLY. Actual App preparation/broker joins and retained
/// fork-source dependency proofs are independent gates before ledger retirement.
#[derive(Clone)]
pub struct SealedSeedTaskUse {actual:Arc<SealInner>}
impl SeedTaskUseGate {
    pub(super) fn new(seed:ObjectRef)->Self {
        let origin=Arc::new(Origin {seed,nonce:uuid::Uuid::new_v4()});
        let permits=||Permits {dispatch:Some(Permit {_origin:origin.clone()}),fork:Some(Permit {_origin:origin.clone()}),
            helper:Some(Permit {_origin:origin.clone()}),enrollment:Some(Permit {_origin:origin.clone()})};
        let open=permits();Self {inner:Arc::new(Inner {origin,state:Mutex::new(StartState {open,
            claimed:Permits {dispatch:None,fork:None,helper:None,enrollment:None},factory:None,cleanup:None,seal:None})})}
    }
    pub fn bind_original_factory(&self,worker:&OwnedWorkerJoin)->Result<()> {
        let mut held=self.inner.state.try_lock().map_err(|_|anyhow::anyhow!("original start gate busy/poisoned"))?;
        ensure!(held.factory.is_none()&&held.seal.is_none(),"original factory already bound/sealed");held.factory=Some(worker.clone());Ok(())
    }
    pub fn factory_is_bound(&self)->bool {self.inner.state.try_lock().ok().is_some_and(|held|held.factory.is_some())}
    pub fn claim_dispatch(&self)->Result<()> {
        let mut held=self.inner.state.try_lock().map_err(|_|anyhow::anyhow!("start gate busy/poisoned"))?;
        ensure!(held.factory.is_some()&&held.seal.is_none(),"original source factory unbound or sealed");
        ensure!(held.factory.as_ref().unwrap().try_joined()?.is_some(),"original factory/TLS not actually joined before dispatch");
        let permit=held.open.dispatch.take().context("original dispatch permit already consumed")?;held.claimed.dispatch=Some(permit);Ok(())
    }
    pub fn claim_fork_creation(&self)->Result<()> {
        let mut held=self.inner.state.try_lock().map_err(|_|anyhow::anyhow!("fork gate busy/poisoned"))?;
        ensure!(held.claimed.dispatch.is_some()&&held.seal.is_none(),"original dispatch absent/sealed");
        let permit=held.open.fork.take().context("original fork permit already consumed")?;held.claimed.fork=Some(permit);Ok(())
    }
    pub fn claim_helper_start(&self)->Result<()> {
        let mut held=self.inner.state.try_lock().map_err(|_|anyhow::anyhow!("helper gate busy/poisoned"))?;
        ensure!(held.claimed.dispatch.is_some()&&held.seal.is_none(),"original dispatch absent/sealed");
        let permit=held.open.helper.take().context("original helper permit already consumed")?;held.claimed.helper=Some(permit);Ok(())
    }
    pub fn claim_enrollment(&self)->Result<()> {
        let mut held=self.inner.state.try_lock().map_err(|_|anyhow::anyhow!("enrollment gate busy/poisoned"))?;
        ensure!(held.claimed.helper.is_some()&&held.seal.is_none(),"original helper admission absent/sealed");
        let permit=held.open.enrollment.take().context("original enrollment permit already consumed")?;held.claimed.enrollment=Some(permit);Ok(())
    }
    pub fn seal_retaining(&self,out:&mut Option<SealedSeedTaskUse>,original:&FinalizationBudget,cancelled:&AtomicBool)->Result<()> {
        start_time(original,cancelled)?;
        let mut held=self.inner.state.try_lock().map_err(|_|anyhow::anyhow!("original start gate busy/poisoned"))?;
        if let Some(cleanup)=&held.cleanup {ensure!(cleanup.constraint()==original.constraint(),"never-started original cleanup clock changed");cleanup.remaining()?;}
        else {held.cleanup=Some(original.clone());}
        if let Some(actual)=&held.seal {
            if let Some(previous)=out.as_ref(){ensure!(Arc::ptr_eq(&previous.actual,actual),"foreign never-started output already retained");}
            else {*out=Some(SealedSeedTaskUse {actual:actual.clone()});}
            drop(held);start_time(original,cancelled)?;return Ok(());
        }
        ensure!(out.is_none()&&held.open.helper.is_some()&&held.open.enrollment.is_some(),"helper/enrollment was attempted; original owner retained");
        let joined=held.factory.as_ref().context("original factory join owner absent")?.try_joined()?.context("original factory/TLS not actually joined")?;
        // All fallible validation precedes the permit moves. This actual gate
        // retains the same seal even after failed delivery or a late postcheck.
        let actual=Arc::new(SealInner {gate:Arc::downgrade(&self.inner),origin:self.inner.origin.clone(),_factory:joined,
            _helper:held.open.helper.take().unwrap(),_enrollment:held.open.enrollment.take().unwrap(),
            dispatch:held.open.dispatch.take(),fork:held.open.fork.take()});
        held.seal=Some(actual.clone());*out=Some(SealedSeedTaskUse {actual});drop(held);
        start_time(original,cancelled)?;Ok(())
    }
    pub fn cleanup_budget(&self)->Option<FinalizationBudget> {
        self.inner.state.try_lock().ok().filter(|held|held.seal.is_some())?.cleanup.clone()
    }
    pub fn original_seal(&self)->Option<SealedSeedTaskUse> {
        self.inner.state.try_lock().ok()?.seal.as_ref().map(|actual|SealedSeedTaskUse {actual:actual.clone()})
    }
    pub fn matches_seal(&self,proof:&SealedSeedTaskUse)->bool {proof.actual.gate.upgrade().is_some_and(|gate|Arc::ptr_eq(&gate,&self.inner))
        &&Arc::ptr_eq(&proof.actual.origin,&self.inner.origin)}
    pub fn same_origin(&self,other:&Self)->bool {Arc::ptr_eq(&self.inner,&other.inner)}
}
impl SealedSeedTaskUse {
    pub fn before_dispatch(&self)->bool {self.actual.dispatch.is_some()}
    pub fn fork_was_never_claimed(&self)->bool {self.actual.fork.is_some()}
    pub fn seed(&self)->&ObjectRef {&self.actual.origin.seed}
    pub fn check_original(&self,gate:&SeedTaskUseGate)->Result<()> {ensure!(gate.matches_seal(self),"never-started proof belongs to another original gate");Ok(())}
    pub fn identity(&self)->uuid::Uuid {self.actual.origin.nonce} // DATA only.
}

fn start_time(original:&FinalizationBudget,cancelled:&AtomicBool)->Result<()> {if cancelled.load(Ordering::Acquire){original.cancel();anyhow::bail!("original never-started settlement cancelled");}original.remaining()?;Ok(())}
