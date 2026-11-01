//! BIN-only actual source caller. Root transport is not a second source factory.
#![cfg(target_os="linux")]
use anyhow::{ensure,Context,Result};
use rudder_native::engine_registration::{RegisteredEngineOwner,FreshProjectPreparation,EngineOperation,EngineOperationKind,RootUploadTransfer,
    ImagePreseedBudget,PublishedImagePreseed,EngineOperationAck};
use rudder_engine::{execution::finalization_budget::FinalizationBudget,protocol::ObjectRef};
use crate::{managed_fresh_genesis::FreshGenesisExport,managed_runtime_image_preseed::ImagePreseedExport};
use std::sync::{Arc,atomic::{AtomicBool,Ordering}};

/// SAME original image export + actual authenticated publication. No JSON,
/// path, boolean or saved receipt constructor. Full graph remains retained.
pub(crate) struct ManagedPublishedImage {source:Arc<ImagePreseedExport>,publication:Arc<PublishedImagePreseed>,adopted:AtomicBool}
impl ManagedPublishedImage {
    fn current_fresh(&self,original:&FinalizationBudget,cancel:&AtomicBool)->Result<()> {
        ensure!(self.adopted.load(Ordering::Acquire),"actual image publication adoption incomplete");self.publication.assert_current()?;self.source.assert_current_for_fresh(original,cancel)?;
        ensure!(self.publication.manifest()==self.source.manifest()&&self.publication.installer_input()==self.source.installer_input(),"image publication is not same actual exported graph");Ok(())
    }
}
enum Source {
    Image {source:Arc<ImagePreseedExport>,original:ImagePreseedBudget,image_receipt_sha256:String},
    Fresh {source:Arc<FreshGenesisExport>,preparation:Arc<FreshProjectPreparation>,image:Arc<ManagedPublishedImage>,original:FinalizationBudget},
}
impl Source {
    fn current(&self,cancel:&AtomicBool)->Result<()> {match self {
        Self::Image {source,original,..}=>source.assert_current(original,cancel),
        Self::Fresh {source,preparation,image,original}=>{preparation.assert_current()?;image.current_fresh(original,cancel)?;source.assert_current(original,cancel)}
    }}
    fn deep(&self,cancel:&AtomicBool)->Result<()> {match self {Self::Image {source,original,..}=>source.assert_current_deep(original,cancel),_=>self.current(cancel)}}
    fn time(&self)->Result<()> {match self {Self::Image {original,..}=>{original.remaining()?;},Self::Fresh {original,..}=>{original.remaining()?;}}Ok(())}
    fn complete(&self)->&[ObjectRef] {match self {Self::Image {source,..}=>source.objects(),Self::Fresh {source,..}=>source.objects()}}
    fn read(&self,r:&ObjectRef,offset:u64,cancel:&AtomicBool)->Result<Vec<u8>> {match self {
        Self::Image {source,original,..}=>source.read_verified_chunk(r,offset,16384,original,cancel),
        Self::Fresh {source,original,..}=>source.read_verified_chunk(r,offset,16384,original,cancel),
    }}
    fn begin(&self,workspace_id:&str,image_batch:bool)->Result<EngineOperationKind> {Ok(match self {
        Self::Image {source,image_receipt_sha256,..}=>if image_batch {EngineOperationKind::BeginImagePreseedV2 {workspace_id:workspace_id.into(),manifest:source.manifest().clone(),installer_input:source.installer_input().clone(),image_receipt_sha256:image_receipt_sha256.clone()}}else{EngineOperationKind::BeginImagePreseedV1 {workspace_id:workspace_id.into(),manifest:source.manifest().clone(),installer_input:source.installer_input().clone(),image_receipt_sha256:image_receipt_sha256.clone()}},
        Self::Fresh {source,preparation,..}=>{let b=source.binding();ensure!(b.workspace_id==workspace_id&&b.prepare_id==preparation.prepare().request_id,"fresh source original preparation differs");
            EngineOperationKind::BeginFreshRootsV1 {workspace_id:b.workspace_id.clone(),prepare_id:b.prepare_id.clone(),export_id:b.export_id.clone(),head:b.head.clone(),source:b.source.clone(),environment:b.environment.clone()}}
    })}
    fn operation(&self,registration:&Arc<RegisteredEngineOwner>,out:&mut Option<EngineOperation>,op:EngineOperationKind,key:String,cancel:Arc<AtomicBool>)->Result<bool> {match self {
        Self::Image {original,..}=>registration.try_begin_image_operation_retaining(out,op,key,original.clone(),cancel),
        Self::Fresh {original,..}=>registration.try_begin_operation_retaining(out,op,key,original.clone(),cancel),
    }}
}
#[derive(Clone,Copy,PartialEq,Eq)]
enum Stage {Begin,Ready,Query,QueryWait,ObjectStart,ObjectWrite,ObjectAck,BatchStart,BatchHeader,BatchWrite,BatchAck,Finish,FinishWait,Publish,PublishWait,Complete}
/// DATA observation of actual producer state only. It grants no permission,
/// ACK/completion or elapsed-time credit, and cannot change any original clock.
#[derive(Clone,Copy,PartialEq,Eq)]
pub(crate) struct ManagedRootUploadProgress {stage:Stage,queried:usize,missing:usize,object:usize,offset:u64,transport:Option<(usize,usize,u64,usize)>}
struct ImageBatchPlan{start:usize,end:usize,ordinal:u32,batch_id:String}
/// Original worker storage must retain this carrier BEFORE first poll and
/// through its actual original worker/TLS join, including queue/spawn/panic/
/// shutdown/error paths. No detached IO, Drop cleanup or automatic replay.
pub(crate) struct ManagedRootUpload {
    source:Source,registration:Arc<RegisteredEngineOwner>,cancelled:Arc<AtomicBool>,workspace_id:String,key:String,
    begin:Option<EngineOperation>,publish:Option<EngineOperation>,stream:Option<RootUploadTransfer>,
    refs:Vec<ObjectRef>,next_batch:usize,missing:Vec<ObjectRef>,next_object:usize,offset:u64,stage:Stage,
    image_batch:bool,batch_ordinal:u32,batch_plan:Option<ImageBatchPlan>,image_publication:Option<Arc<PublishedImagePreseed>>,published:Option<EngineOperationAck>,failed:bool,
}
impl ManagedRootUpload {
    pub(crate) fn begin_image_retaining(out:&mut Option<Self>,source:Arc<ImagePreseedExport>,registration:Arc<RegisteredEngineOwner>,workspace_id:String,
        image_receipt_sha256:String,key:String,original:ImagePreseedBudget,cancelled:Arc<AtomicBool>)->Result<()> {
        ensure!(out.is_none(),"image uploader carrier occupied");
        *out=Some(Self::stage(Source::Image {source,original,image_receipt_sha256},registration,workspace_id,key,cancelled));{let held=out.as_mut().unwrap();let result=held.prepare_refs();if result.is_err(){held.failed=true;}result}
    }
    pub(crate) fn begin_fresh_retaining(out:&mut Option<Self>,source:Arc<FreshGenesisExport>,preparation:Arc<FreshProjectPreparation>,image:Arc<ManagedPublishedImage>,
        key:String,original:FinalizationBudget,cancelled:Arc<AtomicBool>)->Result<()> {
        ensure!(out.is_none(),"fresh uploader carrier occupied");let workspace_id=source.binding().workspace_id.clone();let registration=preparation.registration().clone();
        *out=Some(Self::stage(Source::Fresh {source,preparation,image,original},registration,workspace_id,key,cancelled));{let held=out.as_mut().unwrap();let result=held.prepare_refs();if result.is_err(){held.failed=true;}result}
    }
    fn stage(source:Source,registration:Arc<RegisteredEngineOwner>,workspace_id:String,key:String,cancelled:Arc<AtomicBool>)->Self {
        Self {source,registration,cancelled,workspace_id,key,begin:None,publish:None,stream:None,refs:Vec::new(),next_batch:0,missing:Vec::new(),next_object:0,offset:0,
            stage:Stage::Begin,image_batch:false,batch_ordinal:0,batch_plan:None,image_publication:None,published:None,failed:false}
    }
    fn prepare_refs(&mut self)->Result<()> {
        self.current()?;self.source.deep(&self.cancelled)?;ensure!(!self.key.is_empty()&&self.key.len()<=104&&self.key.bytes().all(|b|b.is_ascii_alphanumeric()||b"-_:.".contains(&b)),"upload stable command key invalid");
        let complete=self.source.complete();ensure!(!complete.is_empty()&&complete.len()<=100000&&complete.windows(2).all(|v|v[0]<v[1]),"actual export closure not complete ordered unique bounded graph");
        let all:std::collections::BTreeSet<_>=complete.iter().collect();let mut bytes=0u64;
        for r in complete {r.validate()?;bytes=bytes.checked_add(r.size_bytes).context("upload graph byte overflow")?;ensure!((r.size_bytes<=4194304||match &self.source{Source::Image {source,..}=>r==source.manifest()&&r.size_bytes<=33554432,Source::Fresh {image,..}=>r==image.source.manifest()&&r.size_bytes<=33554432})&&bytes<=2147483648,"actual complete graph exceeds image scope");}
        match &self.source {
            Source::Image {source,..}=>{ensure!(all.contains(source.manifest())&&all.contains(source.installer_input()),"actual image graph omitted manifest/installer root");
                self.image_batch=self.registration.image_batch_transport()?;
                if self.image_batch{let mut ordered=complete.iter().map(|r|Ok((RootUploadTransfer::image_batch_reference_key(r)?,r.clone()))).collect::<Result<Vec<_>>>()?;ordered.sort_by(|a,b|a.0.as_bytes().cmp(b.0.as_bytes()));ensure!(ordered.windows(2).all(|w|w[0].0.as_bytes()<w[1].0.as_bytes()),"batch full inventory duplicated");self.refs=ordered.into_iter().map(|(_,r)|r).collect();}else{self.refs=complete.to_vec();}},
            Source::Fresh {source,image,..}=>{ensure!(Arc::ptr_eq(&self.registration,image.publication.registration()),"fresh image publication registration differs");
                ensure!(all.contains(&source.binding().source)&&all.contains(&source.binding().environment),"actual fresh export omitted source/environment roots");
                // ONLY original server-published graph paired to SAME retained
                // actual image export can be subtracted. Full source stays held.
                self.refs=complete.iter().filter(|r|image.source.objects().binary_search(r).is_err()).cloned().collect();}
        }
        self.current()
    }
    fn current(&self)->Result<()> {ensure!(!self.failed&&!self.cancelled.load(Ordering::Acquire),"upload retained after failure/cancel");self.registration.assert_current()?;self.source.time()}
    pub(crate) fn progress(&self)->ManagedRootUploadProgress {ManagedRootUploadProgress{stage:self.stage,queried:self.next_batch,missing:self.missing.len(),object:self.next_object,offset:self.offset,transport:self.stream.as_ref().filter(|_|self.image_batch).map(RootUploadTransfer::image_batch_progress)}}
    /// One bounded nonblocking transport step; actual source/CAS reads happen
    /// OFF App under original budget and remain owned by this worker carrier.
    pub(crate) fn poll_retaining(&mut self)->Result<bool> {
        let r=self.poll_inner();if r.is_err(){self.failed=true;}r
    }
    fn poll_inner(&mut self)->Result<bool> {
        self.current()?;
        match self.stage {
            Stage::Begin=>{if self.begin.is_none(){self.source.deep(&self.cancelled)?;let op=self.source.begin(&self.workspace_id,self.image_batch)?;if !self.source.operation(&self.registration,&mut self.begin,op,format!("{}.begin",self.key),self.cancelled.clone())?{return Ok(false);}}
                if matches!(&self.source,Source::Image {..}) {
                    if self.begin.as_mut().unwrap().take_image_begin_retaining(&mut self.stream,&mut self.image_publication)?{
                        self.stage=if self.image_publication.is_some(){Stage::Complete}else{Stage::Ready};
                    }
                } else if self.begin.as_mut().unwrap().take_upload_retaining(&mut self.stream)?{self.stage=Stage::Ready;}},
            Stage::Ready=>{if self.stream.as_mut().unwrap().poll_ready_retaining()?{
                ensure!((self.stream.as_ref().unwrap().scope_data().version==2)==self.image_batch,"Root upload negotiation differs from actual configured request");self.stage=if self.image_batch{Stage::BatchStart}else{Stage::Query};}},
            Stage::Query=>{if self.next_batch==self.refs.len(){self.stage=Stage::ObjectStart;}else {let end=(self.next_batch+128).min(self.refs.len());
                self.stream.as_mut().unwrap().queue_missing_retaining(&self.refs[self.next_batch..end])?;self.next_batch=end;self.stage=Stage::QueryWait;}},
            Stage::QueryWait=>{let mut a=None;if self.stream.as_mut().unwrap().poll_missing_retaining(&mut a)?{self.missing.extend(a.unwrap().missing);self.stage=Stage::Query;}},
            Stage::ObjectStart=>{if self.next_object==self.missing.len(){self.stage=Stage::Finish;}else {self.stream.as_mut().unwrap().queue_object_retaining(self.missing[self.next_object].clone())?;self.offset=0;self.stage=Stage::ObjectWrite;}},
            Stage::ObjectWrite=>{if self.stream.as_mut().unwrap().poll_write_retaining()? {let r=&self.missing[self.next_object];if self.offset==r.size_bytes{self.stage=Stage::ObjectAck;}else{
                let b=self.source.read(r,self.offset,&self.cancelled)?;ensure!(!b.is_empty()&&b.len()<=16384&&self.offset+b.len() as u64<=r.size_bytes,"actual retained object read incomplete/overflow");
                self.offset+=b.len() as u64;self.stream.as_mut().unwrap().queue_bytes_retaining(b)?;}}},
            Stage::ObjectAck=>{if self.stream.as_mut().unwrap().poll_object_retaining()?{self.next_object+=1;self.stage=Stage::ObjectStart;}},
            Stage::BatchStart=>{if self.next_batch==self.refs.len(){self.stage=Stage::Finish;}else{
                if self.batch_plan.is_none(){let Source::Image{source,..}=&self.source else{return Err(anyhow::anyhow!("batch requires original image export"));};let end=image_batch_end(&self.refs,self.next_batch,source.manifest())?;self.batch_plan=Some(ImageBatchPlan{start:self.next_batch,end,ordinal:self.batch_ordinal,batch_id:uuid::Uuid::new_v4().to_string()});}
                let p=self.batch_plan.as_ref().unwrap();self.stream.as_mut().unwrap().queue_image_batch_retaining(self.refs[p.start..p.end].to_vec(),p.ordinal,p.batch_id.clone())?;self.next_object=p.start;self.offset=0;self.stage=Stage::BatchHeader;
            }},
            Stage::BatchHeader=>{if self.stream.as_mut().unwrap().poll_image_batch_header_retaining()?{self.stage=Stage::BatchWrite;}},
            Stage::BatchWrite=>{if self.stream.as_mut().unwrap().poll_write_retaining()?{
                let end=self.batch_plan.as_ref().context("original batch plan absent")?.end;
                if self.next_object==end{self.stage=Stage::BatchAck;}else{let r=&self.refs[self.next_object];
                    if self.offset==r.size_bytes{self.next_object+=1;self.offset=0;}else{
                        let b=self.source.read(r,self.offset,&self.cancelled)?;ensure!(!b.is_empty()&&b.len()<=16384&&self.offset+b.len() as u64<=r.size_bytes,"actual batch source read incomplete/overflow");self.offset+=b.len() as u64;self.stream.as_mut().unwrap().queue_image_batch_bytes_retaining(b)?;
                    }
                }
            }},
            Stage::BatchAck=>{if self.stream.as_mut().unwrap().poll_image_batch_ack_retaining()?{
                self.next_batch=self.batch_plan.as_ref().unwrap().end;self.batch_ordinal+=1;self.batch_plan.take();self.stage=Stage::BatchStart;
            }},
            Stage::Finish=>{self.source.deep(&self.cancelled)?;self.stream.as_mut().unwrap().queue_finish_retaining()?;self.stage=Stage::FinishWait;},
            Stage::FinishWait=>{if self.stream.as_mut().unwrap().poll_finished_retaining()?{self.stage=Stage::Publish;}},
            Stage::Publish=>{self.source.deep(&self.cancelled)?;let transfer_id=self.stream.as_ref().unwrap().scope_data().transfer_id.clone();let op=match &self.source {Source::Image {..}=>EngineOperationKind::PublishImagePreseedV1 {workspace_id:self.workspace_id.clone(),transfer_id},
                Source::Fresh {..}=>EngineOperationKind::PublishFreshRootsV1 {workspace_id:self.workspace_id.clone(),transfer_id}};
                if !self.source.operation(&self.registration,&mut self.publish,op,format!("{}.publish",self.key),self.cancelled.clone())?{return Ok(false);}self.stage=Stage::PublishWait;},
            Stage::PublishWait=>{match &self.source {Source::Image {..}=>{if self.publish.as_mut().unwrap().take_image_publication_retaining(self.stream.as_ref().unwrap(),&mut self.image_publication)?{self.stage=Stage::Complete;}},
                Source::Fresh {source,preparation,..}=>{let p=self.publish.as_mut().unwrap();if p.poll_retaining()?{let ack=p.receipt_data().context("fresh publication ACK absent")?;self.published=Some(ack.clone());
                    ensure!(ack.error.is_none(),"fresh publication rejected; retain original owner");validate_fresh_publication(ack,self.stream.as_ref().unwrap(),&preparation.data().project_id,&source.binding().source,&source.binding().environment)?;p.finish_known_retaining()?;self.stage=Stage::Complete;}}}},
            Stage::Complete=>{self.current()?;self.source.deep(&self.cancelled)?;return Ok(true);},
        }
        self.current()?;Ok(false)
    }
    pub(crate) fn adopt_image_publication_retaining(&self,out:&mut Option<Arc<ManagedPublishedImage>>)->Result<()> {
        ensure!(out.is_none()&&matches!(self.stage,Stage::Complete),"actual image publication incomplete/output occupied");
        let Source::Image {source,..}=&self.source else{return Err(anyhow::anyhow!("uploader is not original image source"));};
        *out=Some(Arc::new(ManagedPublishedImage {source:source.clone(),publication:self.image_publication.as_ref().context("original image publication absent")?.clone(),adopted:AtomicBool::new(false)}));self.current()?;self.source.deep(&self.cancelled)?;out.as_ref().unwrap().adopted.store(true,Ordering::Release);Ok(())
    }
    /// Explicit original acquire after actual source/env/bootstrap publication.
    /// Stages the real operation in caller storage; first send captures15s and
    /// same source30 remains charged. No historical Enroll/readback adoption.
    pub(crate) fn begin_owner_acquire_retaining(&self,out:&mut Option<EngineOperation>,acquire_key:String)->Result<()> {ensure!(self.try_begin_owner_acquire_retaining(out,acquire_key)?,"original engine operation channel busy; no acquire dispatched");Ok(())}
    pub(crate) fn try_begin_owner_acquire_retaining(&self,out:&mut Option<EngineOperation>,acquire_key:String)->Result<bool> {
        ensure!(matches!(self.stage,Stage::Complete)&&self.published.is_some(),"fresh source publication incomplete");self.current()?;self.source.deep(&self.cancelled)?;
        let Source::Fresh {original,..}=&self.source else{return Err(anyhow::anyhow!("image publication cannot acquire a project"));};
        let transfer_id=self.stream.as_ref().context("original transfer absent")?.scope_data().transfer_id.clone();
        let operation=if self.registration.provider_fenced_ownership()?{EngineOperationKind::AcquireProviderFreshOwnerV1 {workspace_id:self.workspace_id.clone(),transfer_id,acquire_key,expected_owner_epoch:"1".into()}}
            else{EngineOperationKind::AcquireFreshOwnerV1 {workspace_id:self.workspace_id.clone(),transfer_id,acquire_key,expected_owner_epoch:"1".into()}};
        self.registration.try_begin_operation_retaining(out,operation,format!("{}.acquire",self.key),original.clone(),self.cancelled.clone())
    }
    pub(crate) fn publication_data(&self)->Option<&EngineOperationAck> {self.published.as_ref()}
    /// Passive observations only after uncertainty. No effect resend, new time,
    /// source/cache drop, project grant or child-exit inference from stream EOF.
    pub(crate) fn observe_only_retaining(&mut self)->Result<()> {if let Some(stream)=&mut self.stream{stream.observe_settlement_retaining()?;}
        for operation in [&mut self.begin,&mut self.publish]{if let Some(op)=operation {if op.receipt_data().is_none(){op.reconcile_only_retaining()?;}}}Ok(())}
}
fn validate_fresh_publication(ack:&EngineOperationAck,stream:&RootUploadTransfer,project_id:&str,source:&ObjectRef,environment:&ObjectRef)->Result<()> {
    let r=ack.result.as_ref().context("fresh publication result absent")?;let o=r.as_object().context("fresh publication result invalid")?;
    let mut names=o.keys().map(String::as_str).collect::<Vec<_>>();names.sort();
    ensure!(names==["bootstrap_receipt","environment_publication","project_id","scope","source_publication","state","transfer_id","workspace_id"],"fresh publication fields invalid");
    ensure!(r["project_id"]==project_id&&r["scope"]=="fresh-bootstrap-only"&&r["state"]=="bootstrapped"&&r["transfer_id"]==stream.scope_data().transfer_id&&r["workspace_id"]==stream.scope_data().workspace_id
        &&!r["bootstrap_receipt"].is_null()&&r["source_publication"]["root"]==serde_json::to_value(source)?&&r["environment_publication"]["root"]==serde_json::to_value(environment)?,"fresh publication original scope/receipts differ");Ok(())
}

// Pure deterministic packing; no Source/Root/lease permission from these refs.
fn image_batch_end(refs:&[ObjectRef],start:usize,manifest:&ObjectRef)->Result<usize>{
    ensure!(start<refs.len(),"batch inventory exhausted");let mut end=start;let mut total=0u64;
    while end<refs.len()&&end-start<1024{let r=&refs[end];
        if r.size_bytes>4194304{ensure!(r==manifest&&r.size_bytes<=33554432,"unexpected oversized image object");if end==start{return Ok(start+1);}break;}
        let next=total.checked_add(r.size_bytes).context("batch packing byte overflow")?;if next>33554432{break;}total=next;end+=1;
    }ensure!(end>start,"batch packing made no progress");Ok(end)
}
#[cfg(test)]#[path="managed_image_batch_client_tests.rs"]mod batch_tests;
