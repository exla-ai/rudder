//! Connected source-only bootstrap after the actual machine receipt open ACK.
//! One machine CAS, original registered owner, <=2 retained finite source jobs.
//! This is bootstrap/cleanup custody, never a project scheduler or model default.
#![cfg(target_os="linux")]
use anyhow::{ensure,Context,Result};
use crate::{managed_machine_cas::ManagedMachineCas,managed_runtime_image_preseed::{ManagedImagePreseed,ImagePreseedExport,ManagedImageBudget},managed_fresh_genesis::{ManagedFreshGenesis,FreshGenesisExport},managed_fresh_assigned::AssignedFreshProject,managed_root_upload::{ManagedRootUpload,ManagedPublishedImage}};
use rudder_native::{host_ingress::HostIngressConfigOwner,engine_registration::{RegisteredEngineOwner,FreshProjectPreparation,EngineOperation,EngineOperationKind,OriginalFreshAcquireAck},managed_owner_lease::ManagedOwnerLease};
use rudder_engine::execution::{finalization_budget::FinalizationBudget,handoff::{OwnedWorkerJoin,WorkerJoined}};
use sha2::{Digest,Sha256};
use std::{any::Any,collections::BTreeMap,sync::{Arc,Mutex,OnceLock,atomic::{AtomicBool,Ordering}},thread,time::Duration};
const MAX_PROJECTS:usize=2;
struct Flow {prepare:Option<EngineOperation>,preparation:Option<Arc<FreshProjectPreparation>>,source:Option<Arc<ManagedFreshGenesis>>,export:Option<Arc<FreshGenesisExport>>,upload:Option<ManagedRootUpload>,acquire:Option<EngineOperation>,ack:Option<Arc<OriginalFreshAcquireAck>>,lease:Option<Arc<ManagedOwnerLease>>,assigned:Option<Arc<Mutex<AssignedFreshProject>>>,error:Option<anyhow::Error>,panic:Option<Box<dyn Any+Send>>}
struct FlowContext {kernel:Arc<crate::managed_machine_entry::ManagedKernelStartup>,machine:Arc<ManagedMachineCas>,image:Arc<ImagePreseedExport>,publication:Arc<ManagedPublishedImage>,config:Arc<HostIngressConfigOwner>}
/// This proof is born only after original actual join/TLS plus a final check of
/// the SAME pre-queue30s. A late join is retained but cannot mint this proof.
struct QualifiedFlowJoined {_joined:WorkerJoined,_original:FinalizationBudget}
struct FlowCell {workspace:String,cancelled:Arc<AtomicBool>,data:Mutex<Flow>,context:OnceLock<FlowContext>,original:OnceLock<FinalizationBudget>,worker:Mutex<Option<OwnedWorkerJoin>>,owned:AtomicBool,qualified:OnceLock<QualifiedFlowJoined>,late_join:Mutex<Option<WorkerJoined>>,denied:AtomicBool}
impl FlowCell {
    fn capture(workspace:String,cancelled:Arc<AtomicBool>)->Arc<Self>{Arc::new(Self{workspace,cancelled,data:Mutex::new(Flow{prepare:None,preparation:None,source:None,export:None,upload:None,acquire:None,ack:None,lease:None,assigned:None,error:None,panic:None}),context:OnceLock::new(),original:OnceLock::new(),worker:Mutex::new(None),owned:AtomicBool::new(false),qualified:OnceLock::new(),late_join:Mutex::new(None),denied:AtomicBool::new(false)})}
    fn start_retaining(self:&Arc<Self>,context:FlowContext)->Result<()> {
        ensure!(self.original.get().is_none()&&self.context.get().is_none(),"source flow original admission already captured");
        let original=FinalizationBudget::begin(Duration::from_secs(30))?;self.original.set(original).map_err(|_|anyhow::anyhow!("original source clock occupied"))?;self.context.set(context).map_err(|_|anyhow::anyhow!("source context occupied"))?;
        let mut slot=self.worker.try_lock().map_err(|_|anyhow::anyhow!("source worker registration busy"))?;ensure!(slot.is_none(),"source original worker already registered");let cell=self.clone();
        let worker=thread::Builder::new().name("rudder-fresh-project-flow".into()).spawn(move||{while !cell.owned.load(Ordering::Acquire){thread::yield_now();}
            let mut held=cell.data.lock().unwrap_or_else(|p|p.into_inner());let result=std::panic::catch_unwind(std::panic::AssertUnwindSafe(||run_flow(&mut held,cell.context.get().unwrap(),cell.original.get().unwrap(),&cell.cancelled)));
            match result{Ok(Ok(()))=>{},Ok(Err(error))=>held.error=Some(error),Err(panic)=>held.panic=Some(panic)}
        })?;*slot=Some(OwnedWorkerJoin::new(worker));self.owned.store(true,Ordering::Release);Ok(())
    }
    fn poll_join_retaining(&self)->Result<bool>{if self.qualified.get().is_some(){return Ok(true);}let slot=self.worker.lock().unwrap_or_else(|p|p.into_inner());if self.qualified.get().is_some(){return Ok(true);}let Some(worker)=slot.as_ref()else{return Ok(false);};worker.start_join_observer()?;
        let Some(joined)=worker.try_joined()?else{return Ok(false);};let original=self.original.get().context("original source clock absent")?;
        let current=time(original,&self.cancelled).is_ok();let held=self.data.lock().unwrap_or_else(|p|p.into_inner());let known=current&&!self.denied.load(Ordering::Acquire)&&held.error.is_none()&&held.panic.is_none()&&held.assigned.is_some();
        if known{time(original,&self.cancelled)?;let _=self.qualified.set(QualifiedFlowJoined{_joined:joined,_original:original.clone()});Ok(true)}else{self.denied.store(true,Ordering::Release);*self.late_join.lock().unwrap_or_else(|p|p.into_inner())=Some(joined);if let Some(assigned)=&held.assigned{assigned.lock().unwrap_or_else(|p|p.into_inner()).close_admission();}Ok(false)}
    }
}
struct Owner {kernel:Option<Arc<crate::managed_machine_entry::ManagedKernelStartup>>,config:Arc<HostIngressConfigOwner>,registration:Arc<RegisteredEngineOwner>,admission:FinalizationBudget,cancelled:Arc<AtomicBool>,cas_capture:Option<ManagedMachineCas>,cas:Option<Arc<ManagedMachineCas>>,image_budget:Option<ManagedImageBudget>,image_source:Option<Arc<ManagedImagePreseed>>,image_export:Option<Arc<ImagePreseedExport>>,image_upload:Option<ManagedRootUpload>,published_image:Option<Arc<ManagedPublishedImage>>,attempted:bool,error:Option<anyhow::Error>,panic:Option<Box<dyn Any+Send>>}
/// All mutable carriers are OUTSIDE catches and remain owned independently of
/// delivery/WS state. No Drop unpin, namespace deletion or implicit retry.
pub(crate) struct ManagedFreshMachine {cancelled:Arc<AtomicBool>,flows:BTreeMap<String,Arc<FlowCell>>,owner:Mutex<Owner>,worker:Mutex<Option<OwnedWorkerJoin>>,owned:AtomicBool,joined:Mutex<Option<WorkerJoined>>}
impl ManagedFreshMachine {
    pub(crate) fn capture(config:Arc<HostIngressConfigOwner>,registration:Arc<RegisteredEngineOwner>,admission:FinalizationBudget)->Arc<Self>{Self::capture_inner(config,registration,None,admission)}
    pub(crate) fn capture_with_kernel(config:Arc<HostIngressConfigOwner>,registration:Arc<RegisteredEngineOwner>,kernel:Arc<crate::managed_machine_entry::ManagedKernelStartup>,admission:FinalizationBudget)->Arc<Self>{Self::capture_inner(config,registration,Some(kernel),admission)}
    fn capture_inner(config:Arc<HostIngressConfigOwner>,registration:Arc<RegisteredEngineOwner>,kernel:Option<Arc<crate::managed_machine_entry::ManagedKernelStartup>>,admission:FinalizationBudget)->Arc<Self>{let cancelled=Arc::new(AtomicBool::new(false));let flows=config.data().projects.iter().map(|p|(p.alias.clone(),FlowCell::capture(p.alias.clone(),cancelled.clone()))).collect();Arc::new(Self{cancelled:cancelled.clone(),flows,owner:Mutex::new(Owner{kernel,config,registration,admission,cancelled,cas_capture:None,cas:None,image_budget:None,image_source:None,image_export:None,image_upload:None,published_image:None,attempted:false,error:None,panic:None}),worker:Mutex::new(None),owned:AtomicBool::new(false),joined:Mutex::new(None)})}
    pub(crate) fn start_retaining(self:&Arc<Self>)->Result<()> {let mut slot=self.worker.try_lock().map_err(|_|anyhow::anyhow!("fresh coordinator registration busy"))?;ensure!(slot.is_none()&&!self.owned.load(Ordering::Acquire),"original fresh coordinator registered");let shared=self.clone();
        let worker=thread::Builder::new().name("rudder-fresh-machine-bootstrap".into()).spawn(move||{while !shared.owned.load(Ordering::Acquire){thread::yield_now();}let mut held=shared.owner.lock().unwrap_or_else(|p|p.into_inner());
            let result=std::panic::catch_unwind(std::panic::AssertUnwindSafe(||held.run_retaining(&shared.flows)));match result{Ok(Ok(()))=>{},Ok(Err(error))=>held.error=Some(error),Err(panic)=>held.panic=Some(panic)}
        })?;*slot=Some(OwnedWorkerJoin::new(worker));self.owned.store(true,Ordering::Release);Ok(())
    }
    pub(crate) fn assigned_off_app(&self,alias:&str,original:&FinalizationBudget)->Result<Option<Arc<Mutex<AssignedFreshProject>>>> {original.remaining()?;let Some(flow)=self.flows.get(alias)else{return Ok(None);};
        if flow.qualified.get().is_none()||flow.denied.load(Ordering::Acquire){return Ok(None);}let held=match flow.data.try_lock(){Ok(h)=>h,Err(std::sync::TryLockError::WouldBlock)=>return Ok(None),Err(_)=>anyhow::bail!("original source flow poisoned")};original.remaining()?;Ok(held.assigned.clone())
    }
    /// Pure, nonblocking denial. This requests no PTY termination and certifies
    /// no helper/backend/actor/seed stop. Keep this whole owner through recovery.
    pub(crate) fn close_admission(&self){self.cancelled.store(true,Ordering::Release);for flow in self.flows.values(){flow.denied.store(true,Ordering::Release);if let Ok(held)=flow.data.try_lock(){if let Some(a)=&held.assigned{if let Ok(a)=a.try_lock(){a.close_admission();}}}}}
    /// OFF App actual original joins only. A false/Err retains all handles and
    /// resources; no data/result/channel signal stands in for TLS completion.
    pub(crate) fn poll_worker_join_off_app(&self)->Result<bool>{for flow in self.flows.values(){let _=flow.poll_join_retaining();}let slot=self.worker.lock().unwrap_or_else(|p|p.into_inner());let Some(worker)=slot.as_ref()else{return Ok(false);};worker.start_join_observer()?;let Some(joined)=worker.try_joined()?else{return Ok(false);};*self.joined.lock().unwrap_or_else(|p|p.into_inner())=Some(joined);Ok(true)}
}
impl Owner {
    fn run_retaining(&mut self,flows:&BTreeMap<String,Arc<FlowCell>>)->Result<()> {
        ensure!(!self.attempted,"fresh machine original bootstrap attempted");self.attempted=true;time(&self.admission,&self.cancelled)?;self.config.assert_current()?;self.registration.assert_current()?;
        // Current source-only genesis retains a legacy host-imported image. A
        // configured baked profile must not start that fallback before a real
        // composed environment/preparation producer is available.
        crate::managed_app::runtime_task::require_legacy_environment_backing(&self.config)?;
        let kernel=self.kernel.as_ref().context("fresh bootstrap requires the actual current kernel startup owner")?.clone();
        ensure!(Arc::ptr_eq(kernel.registration(),&self.registration),"fresh startup belongs to another original registration");kernel.assert_new_startup_off_app(&self.admission,&self.cancelled)?;ensure!(!flows.is_empty()&&flows.len()<=MAX_PROJECTS,"fresh profile supports at most two projects");
        ManagedMachineCas::capture_retaining(&mut self.cas_capture,self.config.clone(),self.registration.clone(),&self.admission,&self.cancelled)?;self.cas=Some(Arc::new(self.cas_capture.take().unwrap()));
        // Actual inert PG permission first; it is not an ACTIVE lease clock.
        for project in &self.config.data().projects{ensure!(fresh_uuid(&project.prepare.namespace.project_id),"fresh bootstrap refuses existing/legacy namespaces");let flow=&flows[&project.alias];let mut held=flow.data.lock().unwrap_or_else(|p|p.into_inner());loop{time(&self.admission,&self.cancelled)?;
            if held.prepare.is_none(){if !self.registration.try_begin_operation_retaining(&mut held.prepare,EngineOperationKind::PrepareFreshProjectV1{workspace_id:flow.workspace.clone()},format!("fp-{}",project.prepare.request_id),self.admission.clone(),self.cancelled.clone())?{pause(&self.admission)?;continue;}}
            let Flow{prepare,preparation,..}=&mut *held;if prepare.as_mut().unwrap().take_preparation_retaining(preparation)?{break;}pause(&self.admission)?;
        }}
        let first=flows.keys().next().unwrap().clone();let policy=self.config.data().image_preseed.as_ref().context("qualified full image policy unconfigured")?.clone();self.image_budget=Some(ManagedImageBudget::begin(Duration::from_secs(300))?);let image=self.image_budget.as_ref().unwrap().clone();
        self.image_source=Some(ManagedImagePreseed::capture(self.cas.as_ref().unwrap().clone(),image.clone(),self.cancelled.clone()));self.image_source.as_ref().unwrap().start_retaining()?;
        loop{image.remaining()?;if self.image_source.as_ref().unwrap().poll_export_retaining(&mut self.image_export)?{break;}image_pause(&image)?;}
        let key=format!("im-{:x}",Sha256::digest(format!("{}:{}",self.config.config_sha256(),policy.full_manifest_sha256).as_bytes()));ManagedRootUpload::begin_image_retaining(&mut self.image_upload,self.image_export.as_ref().unwrap().clone(),self.registration.clone(),first,policy.image_receipt_sha256,key,image.clone(),self.cancelled.clone())?;
        loop{image.remaining()?;let before=self.image_upload.as_ref().unwrap().progress();if self.image_upload.as_mut().unwrap().poll_retaining()?{break;}
            // Yield between bounded successful steps. Sleep only on unchanged
            // actual stage/object/offset; never charge 2ms per accepted16KiB.
            if self.image_upload.as_ref().unwrap().progress()==before{image_pause(&image)?;}else{thread::yield_now();}}
        self.image_upload.as_ref().unwrap().adopt_image_publication_retaining(&mut self.published_image)?;
        // One ORIGINAL30 per job, captured before thread/queue/effects. A failed
        // job keeps its exact worker/resources; no retry or budget reset exists.
        for flow in flows.values(){kernel.check_on_app()?;flow.start_retaining(FlowContext{kernel:kernel.clone(),machine:self.cas.as_ref().unwrap().clone(),image:self.image_export.as_ref().unwrap().clone(),publication:self.published_image.as_ref().unwrap().clone(),config:self.config.clone()})?;
            let work=flow.original.get().unwrap();loop{if flow.poll_join_retaining()?{break;}if flow.denied.load(Ordering::Acquire)||time(work,&flow.cancelled).is_err(){flow.denied.store(true,Ordering::Release);break;}pause(work)?;}
        }Ok(())
    }
}
fn run_flow(flow:&mut Flow,ctx:&FlowContext,work:&FinalizationBudget,cancel:&Arc<AtomicBool>)->Result<()> {
    time(work,cancel)?;ctx.kernel.assert_new_startup_off_app(work,cancel)?;let preparation=flow.preparation.as_ref().context("actual original preparation absent")?.clone();flow.source=Some(ManagedFreshGenesis::capture(preparation.clone(),ctx.machine.clone(),ctx.image.clone(),work.clone(),cancel.clone()));flow.source.as_ref().unwrap().start_retaining()?;
    loop{time(work,cancel)?;if flow.source.as_ref().unwrap().poll_export_retaining(&mut flow.export)?{break;}pause(work)?;}
    ManagedRootUpload::begin_fresh_retaining(&mut flow.upload,flow.export.as_ref().unwrap().clone(),preparation,ctx.publication.clone(),format!("fr-{}",flow.preparation.as_ref().unwrap().prepare().request_id),work.clone(),cancel.clone())?;
    loop{time(work,cancel)?;let before=flow.upload.as_ref().unwrap().progress();if flow.upload.as_mut().unwrap().poll_retaining()?{break;}
        if flow.upload.as_ref().unwrap().progress()==before{pause(work)?;}else{thread::yield_now();}}
    let key=format!("fa-{}",flow.preparation.as_ref().unwrap().prepare().request_id);
    ctx.kernel.assert_new_startup_off_app(work,cancel)?;
    loop{time(work,cancel)?;if flow.acquire.is_none(){if !flow.upload.as_ref().unwrap().try_begin_owner_acquire_retaining(&mut flow.acquire,key.clone())?{pause(work)?;continue;}}
        if flow.acquire.as_mut().unwrap().take_original_acquire_ack_retaining(&mut flow.ack)?{break;}pause(work)?;}
    ManagedOwnerLease::capture_retaining(&mut flow.lease,flow.ack.as_ref().unwrap().clone(),ctx.config.clone())?;flow.assigned=Some(Arc::new(Mutex::new(AssignedFreshProject::capture(flow.source.as_ref().unwrap().clone(),flow.lease.as_ref().unwrap().clone()))));
    ctx.kernel.assert_new_startup_off_app(work,cancel)?;
    flow.assigned.as_ref().unwrap().lock().unwrap_or_else(|p|p.into_inner()).adopt_retaining(work,cancel)
}
pub(crate) fn fresh_config(config:&HostIngressConfigOwner)->bool{config.data().projects.iter().any(|p|fresh_uuid(&p.prepare.namespace.project_id))}
fn fresh_uuid(id:&str)->bool{uuid::Uuid::parse_str(id).is_ok_and(|u|u.get_version_num()==4&&u.to_string()==id)}
fn time(original:&FinalizationBudget,cancel:&AtomicBool)->Result<()> {if cancel.load(Ordering::Acquire){original.cancel();anyhow::bail!("fresh original bootstrap cancelled");}original.remaining()?;Ok(())}
fn pause(original:&FinalizationBudget)->Result<()>{thread::sleep(original.phase_limit(Duration::from_millis(2))?);Ok(())}
fn image_pause(original:&ManagedImageBudget)->Result<()>{thread::sleep(original.remaining()?.min(Duration::from_millis(2)));Ok(())}

#[cfg(test)]
#[path="managed_fresh_machine_join_tests.rs"]
mod join_tests;
