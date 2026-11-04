//! Covered original ingress/fresh roots only. No completion, ledger closure or
//! retirement issuer. All mutations below require the original WorkerCarrier.
use super::*;
use std::{any::Any,marker::PhantomData,sync::{OnceLock,Weak}};
use crate::managed_machine_ingress::{IngressJob,FreshInitializationAttempt,OriginalTaskCommandSelection,OriginalTaskSelectedSource};
use crate::managed_fresh_machine::FlowCell;
use crate::managed_machine_runtime_profile::runtime_environment::SharedRuntimeEnvironment;

const COMMAND_ROOTS:usize=256;
/// Exact original allocation, weak lifetime, and private concrete payload type.
/// Erase only auto-trait traversal: Weak<O> would recursively expand each whole
/// ingress/fresh/seed graph through its own covered continuation. Capture still
/// requires the REAL O:Any+Send+Sync; no unsafe trait assertion or owner grant.
/// The temporary erased Arc shares the original allocation and is not retained.
struct OriginalOwnerWeak<O> {actual:Weak<dyn Any+Send+Sync>,kind:PhantomData<fn()->O>}
impl<O:Any+Send+Sync> OriginalOwnerWeak<O> {
    fn capture(actual:&Arc<O>)->Self {
        let erased:Arc<dyn Any+Send+Sync>=actual.clone();
        Self{actual:Arc::downgrade(&erased),kind:PhantomData}
    }
    fn upgrade(&self)->Option<Arc<O>>{self.actual.upgrade()?.downcast::<O>().ok()}
    fn matches(&self,actual:&Arc<O>)->bool{self.actual.ptr_eq(&Self::capture(actual).actual)}
}
struct Root<O,C> {actual:OriginalOwnerWeak<O>,children:C}
impl<O:Any+Send+Sync,C> Root<O,C> {
    fn belongs(&self,actual:&Arc<O>)->bool{self.actual.matches(actual)}
    fn retain_actual(&self,out:&mut Option<Arc<O>>)->Result<bool> {
        if out.is_none(){*out=self.actual.upgrade();}
        let Some(actual)=out else{return Ok(false)};
        ensure!(self.belongs(actual),"foreign original root owner; captured owner retained");Ok(true)
    }
}
/// Private once-only role, with actual weak payload identity. Retrying a failed
/// post-claim start does not mint another role or erase the original attempt.
struct OriginalRole<O> {actual:OriginalOwnerWeak<O>,started:AtomicBool}
impl<O:Any+Send+Sync> OriginalRole<O> {
    fn capture(actual:&Arc<O>)->Self{Self{actual:OriginalOwnerWeak::capture(actual),started:AtomicBool::new(false)}}
    fn require_actual(&self,actual:&Arc<O>)->Result<()>{ensure!(self.actual.matches(actual),"foreign original covered descendant");Ok(())}
    fn start_once(&self,actual:&Arc<O>)->Result<()>{self.require_actual(actual)?;
        ensure!(!self.started.swap(true,Ordering::AcqRel),"original covered descendant start already attempted; retain same owner");Ok(())}
}
fn charge_role<T>(slot:&OnceLock<Arc<T>>,child:Arc<T>)->Result<Arc<T>> {
    ensure!(slot.get().is_none(),"original covered role already attempted");
    slot.set(child.clone()).map_err(|_|anyhow::anyhow!("original covered role occupied"))?;Ok(child)
}
/// Same production bounded registry used by both roots. It never removes or
/// replaces a charged cell, including failed/unknown original operations.
struct RootSlots<R> {limit:usize,roots:Vec<Arc<R>>}
impl<R> RootSlots<R> {
    fn new(limit:usize)->Self{Self{limit,roots:Vec::new()}}
    fn contains(&self,root:&Arc<R>)->bool{self.roots.iter().any(|r|Arc::ptr_eq(r,root))}
    fn require_member(&self,root:&Arc<R>)->Result<()>{ensure!(self.contains(root),"foreign original producer root");Ok(())}
    fn insert(&mut self,closed:bool,root:Arc<R>)->Result<()> {
        ensure!(!closed,"covered producer roots closed; original owner retained");
        ensure!(!self.contains(&root)&&self.roots.len()<self.limit,"covered original producer root duplicate/full");
        self.roots.push(root);Ok(())
    }
}
struct CommandChildren {task_selected:bool,task:OnceLock<Arc<OriginalTaskCommand>>}
struct FreshChildren {environment:OnceLock<Arc<OriginalFreshEnvironment>>,flows:[OnceLock<Arc<OriginalFreshFlow>>;2]}
pub(crate) struct IngressCommandRoot(Root<IngressJob,CommandChildren>);
pub(crate) struct FreshInitializationRoot(Root<FreshInitializationAttempt,FreshChildren>);
/// Weak links only: a TaskScope cannot retain its enclosing seed or Job.
pub(crate) struct OriginalTaskCommand {root:Weak<IngressCommandRoot>,project:OriginalOwnerWeak<Cell<ProjectCarrier>>,seed:OriginalOwnerWeak<Cell<seeds::SeedCarrier>>}
pub(crate) struct OriginalFreshEnvironment {root:Weak<FreshInitializationRoot>,role:OriginalRole<SharedRuntimeEnvironment>}
pub(crate) struct OriginalFreshFlow {root:Weak<FreshInitializationRoot>,role:OriginalRole<FlowCell>}
pub(super) struct CoveredIngressFreshRoots {closed:bool,commands:RootSlots<IngressCommandRoot>,fresh:RootSlots<FreshInitializationRoot>}
pub(super) struct CoveredProducerRoots {commands:Vec<Arc<IngressCommandRoot>>,fresh:Vec<Arc<FreshInitializationRoot>>}
impl CoveredIngressFreshRoots {
    pub(super) fn new()->Self{Self{closed:false,commands:RootSlots::new(COMMAND_ROOTS),fresh:RootSlots::new(1)}}
    fn reserve_command(&mut self,actual:&Arc<IngressJob>,task_selected:bool)->Result<()> {
        if let Some(root)=actual.covered_root(){ensure!(self.commands.contains(root)&&root.0.belongs(actual),"foreign original command root");return Ok(());}
        let root=Arc::new(IngressCommandRoot(Root{actual:OriginalOwnerWeak::capture(actual),children:CommandChildren{task_selected,task:OnceLock::new()}}));
        self.commands.insert(self.closed,root.clone())?;
        actual.retain_covered_root(root)
    }
    fn reserve_fresh(&mut self,actual:&Arc<FreshInitializationAttempt>)->Result<()> {
        if let Some(root)=actual.covered_root(){ensure!(self.fresh.contains(root)&&root.0.belongs(actual),"foreign original fresh root");return Ok(());}
        let root=Arc::new(FreshInitializationRoot(Root{actual:OriginalOwnerWeak::capture(actual),children:FreshChildren{environment:OnceLock::new(),flows:[OnceLock::new(),OnceLock::new()]}}));
        self.fresh.insert(self.closed,root.clone())?;actual.retain_covered_root(root)
    }
    pub(super) fn close_capture(&mut self)->CoveredProducerRoots {
        self.closed=true;CoveredProducerRoots{commands:self.commands.roots.clone(),fresh:self.fresh.roots.clone()}
    }
    pub(super) fn claim_task(&mut self,selection:&OriginalTaskCommandSelection)->Result<Arc<OriginalTaskCommand>> {
        let (root,actual,project,seed)=selection.originals();
        ensure!(self.commands.contains(root)&&root.0.belongs(actual)&&root.0.children.task_selected,"foreign/non-task original command continuation");
        ensure!(root.0.children.task.get().is_none(),"original command task role already attempted; retain same scope");
        let child=Arc::new(OriginalTaskCommand{root:Arc::downgrade(root),project:OriginalOwnerWeak::capture(&project.cell),seed:OriginalOwnerWeak::capture(&seed.cell)});
        charge_role(&root.0.children.task,child)
    }
    fn require_fresh(&self,root:&Arc<FreshInitializationRoot>)->Result<()> {
        self.fresh.require_member(root)?;ensure!(root.0.actual.upgrade().is_some(),"missing original fresh continuation");Ok(())
    }
    fn claim_environment(&mut self,root:&Arc<FreshInitializationRoot>,actual:&Arc<SharedRuntimeEnvironment>)->Result<Arc<OriginalFreshEnvironment>> {
        self.require_fresh(root)?;
        ensure!(root.0.actual.upgrade().and_then(|a|a.original_coordinator()).is_some_and(|c|c.retains_original_environment(actual)),"environment outside original fresh coordinator");
        ensure!(root.0.children.environment.get().is_none(),"original environment role already attempted");
        let child=Arc::new(OriginalFreshEnvironment{root:Arc::downgrade(root),role:OriginalRole::capture(actual)});
        charge_role(&root.0.children.environment,child)
    }
    fn claim_flow(&mut self,root:&Arc<FreshInitializationRoot>,actual:&Arc<FlowCell>)->Result<Arc<OriginalFreshFlow>> {
        self.require_fresh(root)?;
        ensure!(root.0.actual.upgrade().and_then(|a|a.original_coordinator()).is_some_and(|c|c.retains_original_flow(actual)),"flow outside original fresh coordinator");
        ensure!(!root.0.children.flows.iter().filter_map(OnceLock::get).any(|c|c.role.actual.matches(actual)),"same original flow already attempted");
        let slot=root.0.children.flows.iter().find(|s|s.get().is_none()).context("original fresh flow roles full")?;
        let child=Arc::new(OriginalFreshFlow{root:Arc::downgrade(root),role:OriginalRole::capture(actual)});
        charge_role(slot,child)
    }
}
impl IngressCommandRoot {
    pub(crate) fn original_task_role_charged(&self)->bool{self.0.children.task.get().is_some()}
    pub(crate) fn retain_original_job(&self,out:&mut Option<Arc<IngressJob>>)->Result<bool>{self.0.retain_actual(out)}
    pub(crate) fn same_selected_source(&self,left:&OriginalTaskSelectedSource,right:&OriginalTaskSelectedSource)->bool {
        let(lp,ls)=left.originals();let(rp,rs)=right.originals();
        Arc::ptr_eq(&lp.cell,&rp.cell)&&Arc::ptr_eq(&ls.cell,&rs.cell)
    }
    pub(super) fn matches_original_task_role(self:&Arc<Self>,actual:&Arc<OriginalTaskCommand>,project:&ProjectTicket,seed:&SeedTicket)->bool {
        self.0.children.task.get().is_some_and(|charged|Arc::ptr_eq(charged,actual))
            &&actual.root.upgrade().is_some_and(|root|Arc::ptr_eq(&root,self))&&actual.matches(project,seed)
    }
}
impl OriginalTaskCommand {
    pub(super) fn matches(&self,project:&ProjectTicket,seed:&SeedTicket)->bool {
        self.root.upgrade().is_some()&&self.project.matches(&project.cell)&&self.seed.matches(&seed.cell)
    }
}
impl OriginalFreshEnvironment {
    pub(crate) fn start_once(&self,actual:&Arc<SharedRuntimeEnvironment>)->Result<()> {
        ensure!(self.root.upgrade().is_some(),"original environment root absent");self.role.start_once(actual)
    }
}
impl OriginalFreshFlow {
    pub(crate) fn start_once(&self,actual:&Arc<FlowCell>)->Result<()> {
        ensure!(self.root.upgrade().is_some(),"original flow root absent");self.role.start_once(actual)
    }
}
#[cfg(feature="managed-ingress-genuine-fixture")]
impl IngressCommandRoot {
    pub(crate) fn fixture_no_descendants(&self)->bool {self.0.children.task.get().is_none()}
}
impl FreshInitializationRoot {
    pub(super) fn retain_original_attempt(&self,out:&mut Option<Arc<FreshInitializationAttempt>>)->Result<bool>{self.0.retain_actual(out)}
}
impl CoveredProducerRoots {
    pub(super) fn fresh_roots(&self)->&[Arc<FreshInitializationRoot>]{&self.fresh}
    pub(super) fn command_roots(&self)->&[Arc<IngressCommandRoot>]{&self.commands}
    #[cfg(feature="managed-ingress-genuine-fixture")]
    pub(super) fn fixture_contains_original_command_root(&self,actual:&Arc<IngressCommandRoot>)->bool {
        self.commands.iter().any(|root|Arc::ptr_eq(root,actual))
    }
    pub(super) fn command_count(&self)->usize{self.commands.len()}
    pub(super) fn fresh_count(&self)->usize{self.fresh.len()}
    /// Access owner payloads only AFTER releasing WorkerCarrier. The external
    /// close owner retains the whole ingress. Weak roots alone are not custody.
    pub(super) fn validate_retained(&self,ingress:&crate::managed_machine_ingress::ManagedIngressControl)->Result<bool> {
        for root in &self.commands{let Some(actual)=root.0.actual.upgrade()else{return Ok(false)};
            if !ingress.retains_original_command(&actual)?{return Ok(false)}}
        for root in &self.fresh{let Some(actual)=root.0.actual.upgrade()else{return Ok(false)};
            if !ingress.retains_original_fresh(&actual)?{return Ok(false)}}
        Ok(true)
    }
}
impl MachineControl {
    pub(crate) fn same_original_machine(&self,other:&Self)->bool{Arc::ptr_eq(&self.shared,&other.shared)}
    pub(crate) fn reserve_ingress_root_retaining(&self,actual:&Arc<IngressJob>,original:&FinalizationBudget)->Result<()> {
        ensure!(self.shared.app_thread!=std::thread::current().id(),"covered ingress admission requires off App");
        ensure!(actual.belongs_to_machine(self),"foreign actual ingress machine");let task=actual.is_original_task_command();
        loop{original.remaining()?;match self.shared.carrier.try_lock(){
            Ok(mut c)=>{
                #[cfg(feature="managed-ingress-genuine-fixture")]
                if let Some(gate)=actual.fixture_gate(){gate.before_actual_carrier_charge()?;}
                return c.producer_roots.reserve_command(actual,task)
            },
            Err(TryLockError::WouldBlock)=>std::thread::sleep(original.phase_limit(Duration::from_millis(1))?),
            Err(_)=>anyhow::bail!("original carrier poisoned; command candidate retained")}}
    }
    pub(crate) fn reserve_fresh_root_retaining(&self,actual:&Arc<FreshInitializationAttempt>)->Result<bool> {
        ensure!(self.shared.app_thread!=std::thread::current().id(),"covered fresh admission requires off App");
        ensure!(actual.belongs_to_machine(self),"foreign actual fresh machine");match self.shared.carrier.try_lock(){Ok(mut c)=>{c.producer_roots.reserve_fresh(actual)?;Ok(true)},
            Err(TryLockError::WouldBlock)=>Ok(false),Err(_)=>anyhow::bail!("original carrier poisoned; fresh candidate retained")}
    }
    pub(crate) fn claim_original_fresh_flow(&self,root:&Arc<FreshInitializationRoot>,actual:&Arc<FlowCell>,original:&FinalizationBudget)->Result<Arc<OriginalFreshFlow>> {
        ensure!(self.shared.app_thread!=std::thread::current().id(),"covered flow admission requires off App");
        loop{original.remaining()?;match self.shared.carrier.try_lock(){Ok(mut c)=>return c.producer_roots.claim_flow(root,actual),
            Err(TryLockError::WouldBlock)=>std::thread::sleep(original.phase_limit(Duration::from_millis(1))?),Err(_)=>anyhow::bail!("original carrier poisoned; flow retained")}}
    }
    pub(crate) fn claim_original_fresh_environment(&self,root:&Arc<FreshInitializationRoot>,actual:&Arc<SharedRuntimeEnvironment>,original:&FinalizationBudget)->Result<Arc<OriginalFreshEnvironment>> {
        ensure!(self.shared.app_thread!=std::thread::current().id(),"covered environment admission requires off App");
        loop{original.remaining()?;match self.shared.carrier.try_lock(){Ok(mut c)=>return c.producer_roots.claim_environment(root,actual),
            Err(TryLockError::WouldBlock)=>std::thread::sleep(original.phase_limit(Duration::from_millis(1))?),Err(_)=>anyhow::bail!("original carrier poisoned; environment retained")}}
    }
}
#[cfg(test)]
#[path="managed_producer_reservations_tests.rs"]
mod tests;
