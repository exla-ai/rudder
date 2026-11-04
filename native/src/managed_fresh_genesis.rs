//! Actual source-only genesis, before task/App authority. A genuine original
//! registered preparation receipt and new namespace are required. No selected
//! backend/model/API credential and no helper/PTY/enrollment API exists here.
#![cfg(target_os="linux")]
use anyhow::{ensure,Context,Result};
use crate::{engine_host::FreshLocalNamespace,managed_fresh_environment::ManagedFreshEnvironment,managed_machine_cas::ManagedMachineCas,managed_runtime_image_preseed::ImagePreseedExport};
use rudder_engine::{execution::{finalization_budget::FinalizationBudget,handoff::{OwnedWorkerJoin,WorkerJoined}},protocol::{ObjectKind,ObjectRef},store::LocalStore};
use rudder_native::engine_registration::FreshProjectPreparation;
use rudder_vcs_bridge::initial_source::{PreparedInitialSource,InitialSourceRequest,prepare_initial_source};
use serde::Serialize;
use std::{any::Any,collections::BTreeSet,path::PathBuf,sync::{Arc,Mutex,atomic::{AtomicBool,Ordering}},thread::{self,ThreadId}};
const READ:usize=16*1024;
const MAX_NEW_OBJECTS:usize=128;
const MAX_NEW_BYTES:u64=4*1024*1024;
#[derive(Clone,Serialize)]
pub(crate) struct FreshExportBinding {pub workspace_id:String,pub prepare_id:String,pub export_id:String,pub head:String,pub source:ObjectRef,pub environment:ObjectRef}
struct Owner {preparation:Arc<FreshProjectPreparation>,machine:Arc<ManagedMachineCas>,image:Arc<ImagePreseedExport>,original:FinalizationBudget,cancelled:Arc<AtomicBool>,
    environment_capture:Option<ManagedFreshEnvironment>,environment:Option<Arc<ManagedFreshEnvironment>>,prepared:Option<PreparedInitialSource>,paged:Option<rudder_vcs_bridge::seed::PagedSeedGeneration>,namespace:Option<FreshLocalNamespace>,integration:Option<PathBuf>,seed:Option<PathBuf>,
    backed_runnable:Option<rudder_engine::store::environment_dependencies::RetainedRunnableInputs>,backed_pin_name:Option<String>,git_import:Option<rudder_vcs_bridge::accepted_git_import::RetainedGitImport>,backed_initial:Option<rudder_vcs_bridge::initial_source::backed::BackedInitialEnvironment>,
    prepared_taken:bool,pins:Vec<(String,ObjectRef)>,pin_attempted:usize,pin_acks:usize,binding:Option<FreshExportBinding>,objects:Vec<ObjectRef>,attempted:bool,ready:bool,
    error:Option<anyhow::Error>,panic:Option<Box<dyn Any+Send>>}
/// Actual owner is staged before factory spawn and remains OUTSIDE catch. A
/// rejected result/disconnected transport cannot discard resources or joins.
pub(crate) struct ManagedFreshGenesis {cancelled:Arc<AtomicBool>,owner:Mutex<Owner>,factory:Mutex<Option<OwnedWorkerJoin>>,factory_thread:Mutex<Option<ThreadId>>,owned:AtomicBool,joined:Mutex<Option<WorkerJoined>>,assigned:AtomicBool}
impl ManagedFreshGenesis {
    pub(crate) fn capture(preparation:Arc<FreshProjectPreparation>,machine:Arc<ManagedMachineCas>,image:Arc<ImagePreseedExport>,original:FinalizationBudget,cancelled:Arc<AtomicBool>)->Arc<Self>{
        Arc::new(Self{cancelled:cancelled.clone(),owner:Mutex::new(Owner{preparation,machine,image,original,cancelled,environment_capture:None,environment:None,namespace:None,integration:None,seed:None,prepared:None,paged:None,backed_runnable:None,backed_pin_name:None,git_import:None,backed_initial:None,prepared_taken:false,pins:Vec::new(),pin_attempted:0,pin_acks:0,binding:None,objects:Vec::new(),attempted:false,ready:false,error:None,panic:None}),factory:Mutex::new(None),factory_thread:Mutex::new(None),owned:AtomicBool::new(false),joined:Mutex::new(None),assigned:AtomicBool::new(false)})
    }
    pub(crate) fn start_retaining(self:&Arc<Self>)->Result<()> {
        let mut slot=self.factory.try_lock().map_err(|_|anyhow::anyhow!("original genesis factory registration busy"))?;ensure!(slot.is_none()&&!self.owned.load(Ordering::Acquire),"original genesis factory already registered");let source=self.clone();
        let worker=thread::Builder::new().name("rudder-fresh-source-genesis".into()).spawn(move||{
            while !source.owned.load(Ordering::Acquire){thread::yield_now();}
            let mut held=source.owner.lock().unwrap_or_else(|p|p.into_inner());
            let result=std::panic::catch_unwind(std::panic::AssertUnwindSafe(||->Result<()>{ensure!(source.factory_thread.lock().unwrap_or_else(|p|p.into_inner()).as_ref()==Some(&thread::current().id()),"genesis is not original factory");held.prepare_retaining()}));
            match result{Ok(Ok(()))=>{},Ok(Err(error))=>held.error=Some(error),Err(panic)=>held.panic=Some(panic)}
        })?;
        *self.factory_thread.lock().unwrap_or_else(|p|p.into_inner())=Some(worker.thread().id());*slot=Some(OwnedWorkerJoin::new(worker));self.owned.store(true,Ordering::Release);Ok(())
    }
    pub(crate) fn poll_export_retaining(self:&Arc<Self>,out:&mut Option<Arc<FreshGenesisExport>>)->Result<bool>{
        ensure!(out.is_none(),"original fresh export already adopted");let factory=self.factory.lock().unwrap_or_else(|p|p.into_inner());let factory=factory.as_ref().context("actual original genesis factory absent")?;
        factory.start_join_observer()?;let Some(joined)=factory.try_joined()? else{return Ok(false);};*self.joined.lock().unwrap_or_else(|p|p.into_inner())=Some(joined);
        let held=self.owner.lock().unwrap_or_else(|p|p.into_inner());time(&held.original,&held.cancelled)?;ensure!(held.ready&&held.error.is_none()&&held.panic.is_none(),"genesis failed/unknown; retain original namespace/history");held.current(&held.original,&held.cancelled)?;
        *out=Some(Arc::new(FreshGenesisExport{source:self.clone(),binding:held.binding.as_ref().unwrap().clone(),objects:held.objects.clone(),cached:Mutex::new(None),backed_cached:Mutex::new(None)}));Ok(true)
    }
    fn current(&self,original:&FinalizationBudget,cancel:&AtomicBool)->Result<()> {
        time(original,cancel)?;ensure!(!self.assigned.load(Ordering::Acquire)&&self.joined.lock().unwrap_or_else(|p|p.into_inner()).is_some(),"fresh export requires original genesis/TLS join and no active adoption");
        let held=self.owner.lock().unwrap_or_else(|p|p.into_inner());ensure!(held.ready&&held.error.is_none()&&held.panic.is_none()&&original.constraint()==held.original.constraint(),"original genesis ACK/budget absent or replaced");time(&held.original,&held.cancelled)?;held.current(original,cancel)
    }
    pub(crate) fn assert_assigned_retention(&self)->Result<()> {ensure!(self.assigned.load(Ordering::Acquire)&&self.joined.lock().unwrap_or_else(|p|p.into_inner()).is_some(),"fresh assigned original source/TLS gate absent");let held=self.owner.lock().unwrap_or_else(|p|p.into_inner());ensure!(held.ready&&held.error.is_none()&&held.panic.is_none()&&held.pin_acks==held.pins.len(),"fresh original source ACK incomplete");held.machine.assert_custody()?;held.environment.as_ref().context("fresh environment absent")?.assert_retained_custody()?;held.namespace.as_ref().context("fresh original namespace absent")?.assert_retained_custody()?;
        let pins=held.machine.store()?.retention()?;for (name,root)in &held.pins{ensure!(held.pin_matches(&pins,name,root),"fresh assigned original retention changed");}Ok(())}
    pub(crate) fn promote_engine_retaining(self:&Arc<Self>,out:&mut Option<crate::engine_host::EngineOwnership>,authority:Arc<crate::managed_fresh_assigned::AssignedFreshAuthority>,original:&FinalizationBudget,cancel:&AtomicBool)->Result<()> {
        ensure!(Arc::ptr_eq(authority.source(),self),"assigned adoption differs from SAME original source capsule");self.current(original,cancel)?;let mut held=self.owner.lock().unwrap_or_else(|p|p.into_inner());held.namespace.as_mut().unwrap().promote_engine_retaining(out,authority,original,cancel)?;self.assigned.store(true,Ordering::Release);Ok(())
    }
    pub(crate) fn integration_directory(&self)->Result<PathBuf>{let held=self.owner.lock().unwrap_or_else(|p|p.into_inner());ensure!(self.assigned.load(Ordering::Acquire)&&held.ready,"actual assigned source absent");Ok(held.integration.as_ref().context("actual integration directory absent")?.clone())}
    pub(crate) fn take_prepared_retaining(&self,out:&mut Option<PreparedInitialSource>,lease:&crate::engine_host::EngineSourceLease,journal:&rudder_engine::journal::Journal,original:&FinalizationBudget,cancel:&AtomicBool)->Result<()> {
        time(original,cancel)?;self.assert_assigned_retention()?;lease.check_journal(journal)?;
        {let mut held=self.owner.lock().unwrap_or_else(|p|p.into_inner());ensure!(out.is_none()&&!held.prepared_taken&&held.prepared.is_some()&&lease.owner().project_id==held.preparation.data().project_id,"fresh prepared original already transferred/different owner");
            held.namespace.as_ref().unwrap().check_assigned_writer(journal,lease.owner())?;let state=journal.project_state(&lease.owner().project_id)?;ensure!(state.owner_active&&state.owner==*lease.owner()&&state.integration_head==held.binding.as_ref().unwrap().head,"fresh source no longer exact original accepted HEAD");
            held.prepared_taken=true;*out=held.prepared.take();}
        // Output already owns genuine source/verifier before final fallible checks.
        lease.check_journal(journal)?;self.assert_assigned_retention()?;time(original,cancel)
    }
    pub(crate) fn check_machine_admission(&self)->Result<()> {ensure!(!self.cancelled.load(Ordering::Acquire),"original machine admission closed");Ok(())}
    pub(crate) fn environment(&self)->Result<Arc<ManagedFreshEnvironment>>{let held=self.owner.lock().unwrap_or_else(|p|p.into_inner());ensure!(held.ready,"actual genesis environment unavailable");Ok(held.environment.as_ref().unwrap().clone())}
    pub(crate) fn preparation(&self)->Arc<FreshProjectPreparation>{self.owner.lock().unwrap_or_else(|p|p.into_inner()).preparation.clone()}
}
impl Owner {
    fn pin_matches(&self,state:&rudder_engine::store::RetentionState,name:&str,root:&ObjectRef)->bool{
        if let Some(actual)=&self.backed_runnable{if root==actual.root_data(){return self.backed_pin_name.as_deref()==Some(name)&&actual.matches_record_data(state,name);}}
        state.pins.get(name)==Some(root)
    }
    fn current(&self,original:&FinalizationBudget,cancel:&AtomicBool)->Result<()> {
        time(original,cancel)?;self.preparation.assert_current()?;self.machine.assert_custody()?;if let Some(environment)=&self.environment{environment.current(original,cancel)?;}
        if let Some(namespace)=&self.namespace{namespace.check(original,cancel)?;}
        let retained=self.machine.store()?.retention()?;for (name,root)in self.pins.iter().take(self.pin_attempted){ensure!(self.pin_matches(&retained,name,root),"original fresh source pin absent/changed");}time(original,cancel)
    }
    fn prepare_retaining(&mut self)->Result<()> {
        time(&self.original,&self.cancelled)?;ensure!(!self.attempted,"original genesis already attempted");self.attempted=true;self.preparation.assert_current()?;self.machine.assert_custody()?;
        ensure!(Arc::ptr_eq(self.machine.registration(),self.preparation.registration())&&Arc::ptr_eq(&self.image.source().machine(),&self.machine),"fresh genesis differs from actual registered shared image/CAS owner");
        ManagedFreshEnvironment::capture_retaining(&mut self.environment_capture,self.machine.clone(),self.image.clone(),self.preparation.clone(),&self.original,&self.cancelled)?;
        self.environment=Some(Arc::new(self.environment_capture.take().unwrap()));let environment=self.environment.as_ref().unwrap().clone();
        FreshLocalNamespace::capture_retaining(&mut self.namespace,self.preparation.clone(),environment.clone(),self.original.clone(),self.cancelled.clone())?;
        self.integration=Some(self.namespace.as_mut().unwrap().create_integration_directory()?);self.seed=Some(self.namespace.as_ref().unwrap().store()?.binding().directory.join(format!("genesis-seed-{}",uuid::Uuid::new_v4())));
        let namespace=self.namespace.as_ref().unwrap();let settings=environment.settings()?;
        // The actual original result is retained BEFORE every final check. The
        // synchronous factory also retains all failed output/history paths and
        // input namespace/lock/Journal; no partial result is ever exported.
        let request=InitialSourceRequest{source_git:namespace.original_git()?,private_repository:self.integration.as_ref().unwrap(),seed_destination:self.seed.as_ref().unwrap(),expected_head:namespace.head()?,environment:environment.environment()?,settings:&settings};
        let profile=self.machine.config().data().seed_profile.unwrap_or_default();
        // Genuine NEW genesis has only the locally produced fresh empty commit,
        // not an external canonical pack. Keep its explicit original raw profile;
        // existing-history ingress selects the separate retained PackedHistoryV2.
        let prepared=if let Some(backed)=environment.runtime_backing(){
            let _actual=backed.backing()?;let pin=format!("fresh-runnable:{}:{}",self.preparation.prepare().namespace.project_id,self.preparation.prepare().request_id);
            let subgraph=self.image.environment_subgraph()?.context("actual prepared Environment boundary absent")?;
            ensure!(subgraph.root_data()?==environment.environment()?,"prepared Environment boundary differs from genesis");
            rudder_vcs_bridge::initial_source::backed::BackedInitialEnvironment::stage_subgraph_retaining(&mut self.backed_initial,subgraph,pin,self.original.clone(),self.cancelled.clone())?;
            pollster::block_on(rudder_vcs_bridge::initial_source::backed::prepare_initial_source_backed_history_retaining(self.machine.store()?.clone(),request,profile,rudder_vcs_bridge::accepted_git_import::GitHistoryProfile::RawObjectsV1,&mut self.paged,&mut self.git_import,self.backed_initial.as_mut().unwrap(),&self.original,self.cancelled.clone(),
                ||namespace.check(&self.original,&self.cancelled)))?
        }else{
            pollster::block_on(rudder_vcs_bridge::initial_source::prepare_initial_source_history_with_import_retaining(self.machine.store()?.clone(),request,profile,
                rudder_vcs_bridge::accepted_git_import::GitHistoryProfile::RawObjectsV1,&mut self.paged,&mut self.git_import,&self.original,self.cancelled.clone(),
                ||namespace.check(&self.original,&self.cancelled)))?
        };
        self.backed_runnable=prepared.runnable_inputs();if self.backed_runnable.is_some(){self.backed_pin_name=Some(prepared.runnable_pin.clone());}
        self.prepared=Some(prepared);self.current(&self.original,&self.cancelled)?;let prepared=self.prepared.as_ref().unwrap();
        let id=&self.preparation.prepare().namespace.project_id;let epoch=&self.preparation.prepare().request_id;
        self.pins=vec![(format!("fresh-source:{id}:{epoch}"),prepared.source_root.clone()),(format!("fresh-runnable:{id}:{epoch}"),prepared.runnable_root.clone()),(format!("fresh-seed:{id}:{epoch}"),prepared.seed_root.clone())];
        let store=self.machine.store()?;let stage=store.begin_staging(std::time::Duration::from_secs(120))?;
        ensure!(!store.retention()?.pins.keys().any(|name|self.pins.iter().any(|p|&p.0==name)),"fresh namespaced pin already exists; original recovery required");
        for (name,root)in &self.pins{self.pin_attempted+=1;
            // Runnable/seed already have original verified pins. SourceRoot is
            // a new root extending the genuinely pinned original runnable graph.
            if *root==prepared.runnable_root&&prepared.runnable_inputs().is_some(){
                let actual=prepared.runnable_inputs().unwrap();ensure!(name==&prepared.runnable_pin&&actual.root_data()==root,"actual namespaced backed runnable differs");
                ensure!(actual.matches_record_data(&store.retention()?,name),"actual original backed runnable record changed");
            }else if *root==prepared.source_root&&prepared.runnable_inputs().is_some(){store.publish(&stage.id,name,root)?;}
            else if *root==prepared.source_root{store.publish_extending(&stage.id,name,root,&[prepared.runnable_root.clone()])?;}else{store.pin_existing(name,root)?;}
            self.pin_acks+=1;time(&self.original,&self.cancelled)?;
        }
        let objects=complete_union(store,&prepared.source_root,environment.environment()?,self.image.objects(),&self.original,&self.cancelled)?;self.objects=objects;
        self.binding=Some(FreshExportBinding{workspace_id:self.preparation.data().workspace_id.clone(),prepare_id:self.preparation.prepare().request_id.clone(),export_id:uuid::Uuid::new_v4().to_string(),head:self.namespace.as_ref().unwrap().head()?.0.clone(),source:prepared.source_root.clone(),environment:environment.environment()?.clone()});
        ensure!(self.pin_acks==self.pins.len(),"original fresh pin ACK incomplete");self.current(&self.original,&self.cancelled)?;self.ready=true;Ok(())
    }
}
/// Full source/environment graph. Image objects are taken ONLY from the SAME
/// actual already-verified/pinned image export; no per-project image byte copy.
/// Upload omission still requires ManagedPublishedImage's genuine server ACK.
fn complete_union(store:&LocalStore,source:&ObjectRef,environment:&ObjectRef,image:&[ObjectRef],original:&FinalizationBudget,cancel:&AtomicBool)->Result<Vec<ObjectRef>>{
    ensure!(source.kind==ObjectKind::SourceRoot&&environment.kind==ObjectKind::EnvironmentRoot&&image.windows(2).all(|w|w[0]<w[1]),"fresh roots/image union invalid");
    let mut all:BTreeSet<_>=image.iter().cloned().collect();let mut pending=vec![source.clone(),environment.clone()];let mut count=0usize;let mut bytes=0u64;
    while let Some(reference)=pending.pop(){time(original,cancel)?;if !all.insert(reference.clone()){continue;}reference.validate()?;count+=1;bytes=bytes.checked_add(reference.size_bytes).context("fresh new closure overflow")?;
        ensure!(count<=MAX_NEW_OBJECTS&&bytes<=MAX_NEW_BYTES&&reference.size_bytes<=1024*1024,"fresh new source/environment closure exceeds small profile");let content=store.read_object(&reference)?;reference.verify(&content)?;pending.extend(rudder_engine::store::metadata_references(reference.kind,&content)?);
    }time(original,cancel)?;Ok(all.into_iter().collect())
}
/// No Serde/Clone/receipt constructor. Original factory/TLS and complete actual
/// source, history, namespace, image, CAS and pin owners remain held together.
pub(crate) struct FreshGenesisExport {source:Arc<ManagedFreshGenesis>,binding:FreshExportBinding,objects:Vec<ObjectRef>,cached:Mutex<Option<(ObjectRef,Vec<u8>)>>,backed_cached:Mutex<Option<(ObjectRef,Option<rudder_engine::store::environment_dependencies::RetainedRunnableRead>)>>}
impl FreshGenesisExport {
    pub(crate) fn binding(&self)->&FreshExportBinding{&self.binding}
    pub(crate) fn objects(&self)->&[ObjectRef]{&self.objects}
    pub(crate) fn source(&self)->&Arc<ManagedFreshGenesis>{&self.source}
    pub(crate) fn assert_current(&self,original:&FinalizationBudget,cancel:&AtomicBool)->Result<()>{self.source.current(original,cancel)}
    pub(crate) fn read_verified_chunk(&self,reference:&ObjectRef,offset:u64,max_bytes:usize,original:&FinalizationBudget,cancel:&AtomicBool)->Result<Vec<u8>>{
        self.assert_current(original,cancel)?;ensure!(self.objects.binary_search(reference).is_ok()&&(1..=READ).contains(&max_bytes)&&offset<=reference.size_bytes,"fresh read outside actual retained graph/bounds");
        let held=self.source.owner.lock().unwrap_or_else(|p|p.into_inner());
        if let Some(backing)=held.backed_runnable.clone(){
            let cancelled=held.cancelled.clone();drop(held);let mut slot=self.backed_cached.lock().unwrap_or_else(|p|p.into_inner());
            if slot.as_ref().is_none_or(|(r,_)|r!=reference){if let Some((_,previous))=&*slot{previous.as_ref().context("original backed source read interrupted; retain it")?.verified_bytes()?;}
                *slot=Some((reference.clone(),None));backing.read_retaining(&mut slot.as_mut().unwrap().1,reference.clone(),Arc::new(FreshReadClock{original:original.clone(),cancelled}))?;}
            let bytes=slot.as_ref().unwrap().1.as_ref().context("original backed source read incomplete")?.verified_bytes()?;
            let start=usize::try_from(offset)?;let end=start.saturating_add(max_bytes).min(bytes.len());let chunk=bytes[start..end].to_vec();drop(slot);self.assert_current(original,cancel)?;return Ok(chunk)
        }
        let mut cached=self.cached.lock().unwrap_or_else(|p|p.into_inner());if cached.as_ref().is_none_or(|(r,_)|r!=reference){let bytes=held.machine.store()?.read_object(reference)?;reference.verify(&bytes)?;*cached=Some((reference.clone(),bytes));}
        let bytes=&cached.as_ref().unwrap().1;let start=usize::try_from(offset)?;let end=start.saturating_add(max_bytes).min(bytes.len());let chunk=bytes[start..end].to_vec();drop(cached);drop(held);self.assert_current(original,cancel)?;Ok(chunk)
    }
}
fn time(original:&FinalizationBudget,cancel:&AtomicBool)->Result<()> {if cancel.load(Ordering::Acquire){original.cancel();anyhow::bail!("original fresh source job cancelled");}original.remaining()?;Ok(())}
#[cfg(test)]
#[path="managed_fresh_genesis_tests.rs"]
mod tests;

struct FreshReadClock{original:FinalizationBudget,cancelled:Arc<AtomicBool>}
impl rudder_engine::store::ImageImportFence for FreshReadClock{fn remaining(&self)->Result<std::time::Duration>{time(&self.original,&self.cancelled)?;self.original.remaining()}}

impl ManagedFreshGenesis {
    pub(crate) fn observe_original_producer_worker(&self)->Result<Option<crate::managed_machine_entry::census::WorkerCensus>> {
        crate::managed_machine_entry::census::worker(&self.factory)
    }
}