//! Fixed Root-authenticated engine operations. No generic RPC/Context, root
//! retention booleans, source/task admission or historical lease adoption.
use super::*;
use std::sync::{Mutex,TryLockError};
use rudder_engine::journal::ManagedProjectPrepare;
use rudder_engine::protocol::{ObjectRef,ObjectKind};
#[path="image_preseed_budget.rs"]
mod image_preseed_budget;
pub use image_preseed_budget::{ImagePreseedBudget,ImagePreseedConstraint};
#[path="fresh_root_transfer.rs"]
mod fresh_root_transfer;
pub use fresh_root_transfer::{RootUploadTransfer,MissingObjectsAck};
pub use super::super::host_ingress::root_upload::UploadScope;
use super::super::host_ingress::root_upload::RootUploader;
const OP_MAX:usize=24576;
fn bytes<T:Serialize>(value:&T)->Result<Vec<u8>> {let b=canonical_json(value).map_err(|_|fault())?;ensure!(b.len()<=OP_MAX,"engine operation packet exceeds bound");Ok(b)}
fn sign<T:Serialize>(key:&[u8],domain:&[u8],body:&T)->Result<String>{let mut mac=<Hmac<Sha256> as Mac>::new_from_slice(key).map_err(|_|fault())?;mac.update(domain);mac.update(&bytes(body)?);Ok(hex(&mac.finalize().into_bytes()))}
fn verify<T:Serialize>(key:&[u8],domain:&[u8],packet:&Signed<T>)->Result<()> {let mut mac=<Hmac<Sha256> as Mac>::new_from_slice(key).map_err(|_|fault())?;mac.update(domain);mac.update(&bytes(&packet.body)?);mac.verify_slice(&unhex::<32>(packet.mac.as_bytes())?).map_err(|_|fault())}

const REQUEST_DOMAIN:&[u8]=b"rudder_engine_operation_request_v1\0";
const ACK_DOMAIN:&[u8]=b"rudder_engine_operation_ack_v1\0";
#[derive(Clone,Debug,PartialEq,Eq,Serialize,Deserialize)]
#[serde(tag="op",deny_unknown_fields)]
pub enum EngineOperationKind {
    PrepareFreshProjectV1 {workspace_id:String},FreshProjectStatusV1 {workspace_id:String},EnrollFreshProjectV1 {workspace_id:String},
    BeginFreshRootsV1 {workspace_id:String,prepare_id:String,export_id:String,head:String,source:ObjectRef,environment:ObjectRef},
    PublishFreshRootsV1 {workspace_id:String,transfer_id:String},
    AcquireFreshOwnerV1 {workspace_id:String,transfer_id:String,acquire_key:String,expected_owner_epoch:String},
    BeginImagePreseedV1 {workspace_id:String,manifest:ObjectRef,installer_input:ObjectRef,image_receipt_sha256:String},
    BeginImagePreseedV2 {workspace_id:String,manifest:ObjectRef,installer_input:ObjectRef,image_receipt_sha256:String},
    PublishImagePreseedV1 {workspace_id:String,transfer_id:String},
    RenewFreshOwnerV1 {workspace_id:String,owner_epoch:String,lease_id:String,request_id:String,expected_serial:String},
}
impl EngineOperationKind {
    pub fn workspace_id(&self)->&str {match self {Self::PrepareFreshProjectV1 {workspace_id}|Self::FreshProjectStatusV1 {workspace_id}|Self::EnrollFreshProjectV1 {workspace_id}
        |Self::BeginFreshRootsV1 {workspace_id,..}|Self::PublishFreshRootsV1 {workspace_id,..}|Self::AcquireFreshOwnerV1 {workspace_id,..}
        |Self::BeginImagePreseedV1 {workspace_id,..}|Self::BeginImagePreseedV2 {workspace_id,..}|Self::PublishImagePreseedV1 {workspace_id,..}|Self::RenewFreshOwnerV1 {workspace_id,..}=>workspace_id}}
    fn image(&self)->bool {matches!(self,Self::BeginImagePreseedV1 {..}|Self::BeginImagePreseedV2 {..}|Self::PublishImagePreseedV1 {..})}
    fn begin_upload(&self)->bool {matches!(self,Self::BeginFreshRootsV1 {..}|Self::BeginImagePreseedV1 {..}|Self::BeginImagePreseedV2 {..})}
    fn validate(&self)->Result<()> {match self {
        Self::BeginFreshRootsV1 {prepare_id,export_id,head,source,environment,..}=>{uuid(prepare_id)?;uuid(export_id)?;ensure!(head.len()==40&&head.bytes().all(|b|b.is_ascii_hexdigit()&&!b.is_ascii_uppercase()),"fresh head invalid");source.validate()?;environment.validate()?;
            ensure!(source.kind==ObjectKind::SourceRoot&&environment.kind==ObjectKind::EnvironmentRoot&&source.size_bytes<=1048576&&environment.size_bytes<=1048576,"fresh roots invalid");},
        Self::PublishFreshRootsV1 {transfer_id,..}|Self::PublishImagePreseedV1 {transfer_id,..}=>uuid(transfer_id)?,
        Self::AcquireFreshOwnerV1 {transfer_id,acquire_key,expected_owner_epoch,..}=>{uuid(transfer_id)?;bounded_key(acquire_key)?;ensure!(expected_owner_epoch=="1","fresh acquire epoch invalid");},
        Self::RenewFreshOwnerV1 {owner_epoch,lease_id,request_id,expected_serial,..}=>{uuid(lease_id)?;uuid(request_id)?;let epoch=rudder_engine::fleet_lease::Counter::parse(owner_epoch)?;rudder_engine::fleet_lease::Counter::parse(expected_serial)?;ensure!(epoch.value()>0,"owner epoch invalid");},
        Self::BeginImagePreseedV1 {manifest,installer_input,image_receipt_sha256,..}|Self::BeginImagePreseedV2 {manifest,installer_input,image_receipt_sha256,..}=>{manifest.validate()?;installer_input.validate()?;unhex::<32>(image_receipt_sha256.as_bytes())?;
            ensure!(manifest.kind==ObjectKind::Blob&&installer_input.kind==ObjectKind::Directory&&manifest.size_bytes<=33554432&&installer_input.size_bytes<=4194304,"image roots invalid");},_=>{}}Ok(())}
}
/// Time DATA only; same wire layout as v2. Wider policy is accepted ONLY for
/// the two image operations, never fresh/task/startup or acquisition.
#[derive(Clone,Debug,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationConstraint {format_version:u32,maximum_milliseconds:u32,boottime_deadline_nanoseconds:String}
impl From<FinalizationConstraint> for OperationConstraint {fn from(c:FinalizationConstraint)->Self {serde_json::from_value(serde_json::to_value(c).expect("bounded constraint serializes")).expect("identical constraint layout")}}
impl OperationConstraint {
    fn image(c:ImagePreseedConstraint)->Result<Self> {Ok(serde_json::from_value(serde_json::to_value(c)?)?)}
    fn adopt(&self,image:bool)->Result<OperationBudget> {let b=serde_json::to_vec(self)?;
        if image {Ok(OperationBudget::Image(ImagePreseedBudget::adopt(serde_json::from_slice(&b)?)?))}
        else {Ok(OperationBudget::Fresh(FinalizationBudget::from_constraint(&FinalizationConstraint::from_json(&b)?)?))}}
}
#[derive(Clone)]
pub(crate) enum OperationBudget {Fresh(FinalizationBudget),Image(ImagePreseedBudget)}
impl OperationBudget {
    pub(crate) fn remaining(&self)->Result<std::time::Duration> {match self {Self::Fresh(b)=>b.remaining(),Self::Image(b)=>b.remaining()}}
    pub(crate) fn cancel(&self) {match self {Self::Fresh(b)=>b.cancel(),Self::Image(b)=>b.cancel()}}
    fn constraint(&self)->Result<OperationConstraint> {match self {Self::Fresh(b)=>Ok(b.constraint().into()),Self::Image(b)=>OperationConstraint::image(b.constraint())}}
    fn image(&self)->bool {matches!(self,Self::Image(_))}
}
#[derive(Clone,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeRequest {version:u32,request_nonce:String,command_key:String,operation:EngineOperationKind,constraint:OperationConstraint}
#[derive(Clone,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct OperationBody {version:u32,kind:String,account_id:String,home_id:String,store_id:String,incarnation:String,engine_instance_id:String,boot_id:String,credential_id:String,epoch:String,root_nonce:String,
    registration_proof_sha256:String,request_nonce:String,command_key:String,operation:EngineOperationKind,payload_sha256:String,constraint:OperationConstraint,remaining_milliseconds:u32}
#[derive(Clone,Copy,Debug,PartialEq,Eq,Serialize,Deserialize)]
#[serde(rename_all="snake_case")]
pub enum EngineOperationError {Rejected,Busy,Expired,EffectUnknown,Unconfigured}
#[derive(Clone,Debug,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineOperationAck {pub version:u32,pub kind:String,pub account_id:String,pub home_id:String,pub store_id:String,pub incarnation:String,pub engine_instance_id:String,pub boot_id:String,pub credential_id:String,pub epoch:String,pub root_nonce:String,
    pub registration_proof_sha256:String,pub request_nonce:String,pub command_key:String,pub operation:EngineOperationKind,pub payload_sha256:String,pub constraint:OperationConstraint,pub request_sha256:String,
    pub result:Option<serde_json::Value>,pub error:Option<EngineOperationError>}
#[derive(Clone,Debug,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FreshPreparationData {pub scope:String,pub state:String,pub account_id:String,pub home_id:String,pub store_id:String,pub incarnation:String,pub project_id:String,pub workspace_id:String,pub project_key:String,
    pub prepare:ManagedProjectPrepare,pub engine_instance_id:String,pub boot_id:String,pub assignment_epoch:String}
/// Actual committed preparation ACK + original registration. Not Clone/Serde,
/// source/lease/TaskProject/App authority, or permission to reuse a namespace.
pub struct FreshProjectPreparation {data:FreshPreparationData,registration:Arc<RegisteredEngineOwner>,ack:EngineOperationAck,published:AtomicBool}
impl FreshProjectPreparation {
    pub fn data(&self)->&FreshPreparationData {&self.data}pub fn prepare(&self)->&ManagedProjectPrepare {&self.data.prepare}
    pub fn registration(&self)->&Arc<RegisteredEngineOwner> {&self.registration}pub fn receipt_data(&self)->&EngineOperationAck {&self.ack}
    pub fn assert_current(&self)->Result<()> {ensure!(self.published.load(Ordering::Acquire),"fresh preparation owner not published");self.registration.assert_current()}
}
fn bounded_key(value:&str)->Result<()> {ensure!(!value.is_empty()&&value.len()<=128&&value.bytes().all(|b|b.is_ascii_alphanumeric()||b"-_:.".contains(&b)),"engine operation identity invalid");Ok(())}
fn native_scope(request:&NativeRequest,config:&HostIngressConfig)->Result<()> {ensure!(request.version==1,"engine operation version invalid");uuid(&request.request_nonce)?;bounded_key(&request.command_key)?;bounded_key(request.operation.workspace_id())?;
    ensure!(config.projects.iter().any(|p|p.alias==request.operation.workspace_id()),"engine operation outside original project scope");request.operation.validate()?;
    if let EngineOperationKind::BeginImagePreseedV1 {manifest,image_receipt_sha256,..}|EngineOperationKind::BeginImagePreseedV2 {manifest,image_receipt_sha256,..}=&request.operation {let p=config.image_preseed.as_ref().context("qualified image preseed policy unconfigured")?;
        ensure!(&p.image_receipt_sha256==image_receipt_sha256&&p.full_manifest_sha256==manifest.digest,"image import differs from original qualified config");}
    if matches!(request.operation,EngineOperationKind::BeginImagePreseedV2 {..}){ensure!(config.image_preseed.as_ref().and_then(|p|p.batch_transport.as_deref())==Some("batch-v1"),"V2 image batch transport not explicitly configured");}
    if matches!(request.operation,EngineOperationKind::RenewFreshOwnerV1 {..}) {config.owner_lease_safety.as_ref().context("qualified owner clock policy unconfigured")?.reserve_nanoseconds()?;}
    bytes(request)?;Ok(())}
fn native_request(request:&NativeRequest,config:&HostIngressConfig)->Result<()> {native_scope(request,config)?;request.constraint.adopt(request.operation.image())?.remaining()?;Ok(())}
fn matches_registration(ack:&EngineOperationAck,registration:&RegistrationAck)->Result<()> {let r=&registration.binding;ensure!(ack.version==1&&ack.kind=="engine_operation_ack"&&ack.account_id==r.account_id&&ack.home_id==r.home_id&&ack.store_id==r.store_id
    &&ack.incarnation==r.incarnation&&ack.engine_instance_id==registration.engine_instance_id&&ack.boot_id==registration.boot_id&&ack.credential_id==registration.credential_id&&ack.epoch==r.epoch
    &&ack.root_nonce==registration.root_nonce&&ack.registration_proof_sha256==registration.proof_sha256,"engine operation registration differs");Ok(())}
fn matches_native(ack:&EngineOperationAck,request:&NativeRequest)->Result<()> {ensure!(ack.request_nonce==request.request_nonce&&ack.command_key==request.command_key&&ack.operation==request.operation&&ack.constraint==request.constraint
    &&ack.payload_sha256==hex(&Sha256::digest(bytes(&request.operation)?))&&ack.result.as_ref().is_some_and(|v|!v.is_null())!=ack.error.is_some(),"engine operation original response differs");unhex::<32>(ack.request_sha256.as_bytes())?;Ok(())}
fn matches_signed(ack:&EngineOperationAck,request:&OperationBody,registration:&RegistrationAck)->Result<()> {matches_registration(ack,registration)?;
    matches_native(ack,&NativeRequest {version:1,request_nonce:request.request_nonce.clone(),command_key:request.command_key.clone(),operation:request.operation.clone(),constraint:request.constraint.clone()})?;
    ensure!(ack.request_sha256==hex(&Sha256::digest(bytes(request)?)),"engine operation original signed body differs");Ok(())}
#[derive(Deserialize)]#[serde(deny_unknown_fields)]
struct BeginUploadResult {scope:String,state:String,transfer_id:String,project_id:String,workspace_id:String,upload_path:String,maximum_new_objects:u32,maximum_new_bytes:u64,maximum_object_bytes:u64,#[serde(default)]maximum_manifest_bytes:Option<u64>}
fn upload_scope(ack:&EngineOperationAck,registration:&RegistrationAck)->Result<UploadScope> {
    ensure!(ack.error.is_none()&&ack.operation.begin_upload(),"engine operation did not authorize upload");let r:BeginUploadResult=serde_json::from_value(ack.result.clone().context("upload result absent")?).map_err(|_|fault())?;
    let image=ack.operation.image();let batch=matches!(ack.operation,EngineOperationKind::BeginImagePreseedV2 {..});ensure!(r.scope==(if batch{"image-preseed-batch-transfer-only"}else if image{"image-preseed-transfer-only"}else{"fresh-root-transfer-only"})&&r.state=="uploading"&&r.workspace_id==ack.operation.workspace_id(),"upload Begin result differs");
    uuid(&r.transfer_id)?;uuid(&r.project_id)?;ensure!(r.upload_path==format!("/api/rudder/home/{}/fresh-roots/{}",registration.binding.home_id,r.transfer_id),"upload route differs from fixed original scope");
    let s=UploadScope {version:if batch{2}else{1},home_id:registration.binding.home_id.clone(),workspace_id:r.workspace_id,transfer_id:r.transfer_id,request_nonce:ack.request_nonce.clone(),mode:if batch{"image_batch_v1"}else if image{"image"}else{"fresh"}.into(),
        maximum_objects:r.maximum_new_objects,maximum_bytes:r.maximum_new_bytes,maximum_object_bytes:r.maximum_object_bytes,constraint:ack.constraint.clone(),maximum_manifest_bytes:r.maximum_manifest_bytes,
        manifest:match &ack.operation{EngineOperationKind::BeginImagePreseedV1 {manifest,..}|EngineOperationKind::BeginImagePreseedV2 {manifest,..}=>Some(manifest.clone()),_=>None}};s.validate()?;Ok(s)
}
#[derive(Deserialize)]#[serde(deny_unknown_fields)]
struct FabricRootPublication {format_version:u32,root_id:String,root:ObjectRef,durability:String,published_at_ms:u64,object_count:u64,total_bytes:u64}
impl FabricRootPublication {fn matches(&self,expected:&ObjectRef)->Result<()> {let _published=self.published_at_ms;ensure!(self.format_version==1&&!self.root_id.is_empty()&&self.root==*expected&&self.durability=="replicated"
    &&self.object_count>0&&self.object_count<=100000&&self.total_bytes<=2147483648,"actual Fabric publication differs");Ok(())}}
/// DATA schema only. A reuse owner is minted exclusively after the actual
/// current Root operation/registered peer ACK is matched and carries NO FD.
#[derive(Deserialize)]#[serde(deny_unknown_fields)]
struct ImageReuseResult {scope:String,state:String,transfer_id:String,source_transfer_id:String,project_id:String,workspace_id:String,installer_input:ObjectRef,manifest_publication:FabricRootPublication,publication:FabricRootPublication}
fn has_image_reuse_result(ack:&EngineOperationAck)->bool {matches!(ack.operation,EngineOperationKind::BeginImagePreseedV1 {..}|EngineOperationKind::BeginImagePreseedV2 {..})&&ack.result.as_ref().and_then(|v|v.get("scope")).and_then(|v|v.as_str())==Some("image-preseed-reuse-only")}
fn image_reuse_result(ack:&EngineOperationAck,config:&HostIngressConfig)->Result<ImageReuseResult> {
    ensure!(ack.error.is_none(),"rejected image reuse has no capability");
    let (EngineOperationKind::BeginImagePreseedV1 {workspace_id,manifest,installer_input,image_receipt_sha256}|EngineOperationKind::BeginImagePreseedV2 {workspace_id,manifest,installer_input,image_receipt_sha256})=&ack.operation else {return Err(fault());};
    native_scope(&NativeRequest {version:1,request_nonce:ack.request_nonce.clone(),command_key:ack.command_key.clone(),operation:ack.operation.clone(),constraint:ack.constraint.clone()},config)?;
    let r:ImageReuseResult=serde_json::from_value(ack.result.clone().context("image reuse result absent")?).map_err(|_|fault())?;
    uuid(&r.transfer_id)?;uuid(&r.source_transfer_id)?;uuid(&r.project_id)?;
    let selected=config.projects.iter().find(|p|&p.alias==workspace_id).context("image reuse outside configured project")?;
    ensure!(r.scope=="image-preseed-reuse-only"&&r.state=="published"&&r.workspace_id==*workspace_id&&r.project_id==selected.prepare.namespace.project_id&&r.installer_input==*installer_input&&r.transfer_id!=r.source_transfer_id,"image reuse current scope/graph differs");
    let policy=config.image_preseed.as_ref().context("qualified image policy absent")?;
    ensure!(policy.image_receipt_sha256==*image_receipt_sha256&&policy.full_manifest_sha256==manifest.digest,"image reuse original policy differs");
    r.manifest_publication.matches(manifest)?;r.publication.matches(installer_input)?;
    ensure!(r.manifest_publication.root_id==format!("managed-image-manifest-{}",r.source_transfer_id)&&r.publication.root_id==format!("managed-image-{}",r.source_transfer_id),"image reuse original publication pins differ");Ok(r)
}
/// Authenticated original cold Publish ACK or current Begin reuse ACK, retaining
/// the actual current registered Root exchange. Neither grants source/Task
/// authority; BIN pairs SAME actual local image export before subtracting graph.
pub struct PublishedImagePreseed {registration:Arc<RegisteredEngineOwner>,manifest:ObjectRef,installer_input:ObjectRef,image_receipt_sha256:String,ack:EngineOperationAck,begin:EngineOperationAck,published:AtomicBool}
impl PublishedImagePreseed {pub fn assert_current(&self)->Result<()> {ensure!(self.published.load(Ordering::Acquire),"image publication not adopted");self.registration.assert_current()}
    pub fn registration(&self)->&Arc<RegisteredEngineOwner>{&self.registration}pub fn manifest(&self)->&ObjectRef{&self.manifest}pub fn installer_input(&self)->&ObjectRef{&self.installer_input}
    pub fn image_receipt_sha256(&self)->&str{&self.image_receipt_sha256}pub fn receipt_data(&self)->&EngineOperationAck{&self.ack}pub fn begin_data(&self)->&EngineOperationAck{&self.begin}}
/// Whole original acquire ACK plus clock captured BEFORE first Native->Root
/// send. Historical Enroll/status JSON can never construct this receipt.
/// The PG owner/fence/renewal adapter must still validate/adopt the actual grant.
pub struct OriginalFreshAcquireAck {registration:Arc<RegisteredEngineOwner>,ack:EngineOperationAck,lease:FinalizationBudget,owner_receipt:serde_json::Value,published:AtomicBool}
impl OriginalFreshAcquireAck {pub fn assert_current(&self)->Result<()> {ensure!(self.published.load(Ordering::Acquire),"fresh acquire ACK not adopted");self.lease.remaining()?;self.registration.assert_current()}
    pub fn registration(&self)->&Arc<RegisteredEngineOwner>{&self.registration}pub fn receipt_data(&self)->&EngineOperationAck{&self.ack}pub fn owner_receipt(&self)->&serde_json::Value{&self.owner_receipt}
    pub fn original_lease_budget(&self)->&FinalizationBudget{&self.lease}}
/// Only an actual original Root ACK with its native BEFORE-send clock can
/// construct this proof. Historical/readback JSON cannot reset owner time.
pub struct OriginalOwnerRenewalAck {registration:Arc<RegisteredEngineOwner>,ack:EngineOperationAck,lease:FinalizationBudget,grant:serde_json::Value,published:AtomicBool}
impl OriginalOwnerRenewalAck {pub fn assert_current(&self)->Result<()> {ensure!(self.published.load(Ordering::Acquire),"renewal ACK not adopted");self.lease.remaining()?;self.registration.assert_current()}
    pub fn registration(&self)->&Arc<RegisteredEngineOwner>{&self.registration}pub fn receipt_data(&self)->&EngineOperationAck{&self.ack}
    pub fn owner_grant(&self)->&serde_json::Value{&self.grant}pub fn original_lease_budget(&self)->&FinalizationBudget{&self.lease}}
struct NativeExchange {request:NativeRequest,wire:Vec<u8>,sent:bool,received:Option<storage::Packet>,ack:Option<EngineOperationAck>,validated:bool,uncertain:bool}
pub(super) struct NativeChannel {endpoint:storage::Endpoint,active:Option<NativeExchange>,renewal:Option<NativeExchange>,unclassified:Option<storage::Packet>,fenced:bool}
impl NativeChannel {pub(super) fn capture(fd:OwnedFd)->Result<Self>{Ok(Self {endpoint:storage::Endpoint::capture(fd)?,active:None,renewal:None,unclassified:None,fenced:false})}
    fn lane(&mut self,renewal:bool)->&mut Option<NativeExchange>{if renewal{&mut self.renewal}else{&mut self.active}}
    fn get(&self,nonce:&str)->Option<&NativeExchange>{self.active.as_ref().filter(|v|v.request.request_nonce==nonce).or_else(||self.renewal.as_ref().filter(|v|v.request.request_nonce==nonce))}
    fn get_mut(&mut self,nonce:&str)->Option<&mut NativeExchange>{if self.active.as_ref().is_some_and(|v|v.request.request_nonce==nonce){self.active.as_mut()}else{self.renewal.as_mut().filter(|v|v.request.request_nonce==nonce)}}
    fn take(&mut self,nonce:&str)->Option<NativeExchange>{if self.active.as_ref().is_some_and(|v|v.request.request_nonce==nonce){self.active.take()}else if self.renewal.as_ref().is_some_and(|v|v.request.request_nonce==nonce){self.renewal.take()}else{None}}
}
/// Caller retains through actual worker/result settlement. Drop never frees
/// an unknown active channel slot or retries an operation.
pub struct EngineOperation {registration:Arc<RegisteredEngineOwner>,nonce:String,original:OperationBudget,cancelled:Arc<AtomicBool>,receipt:Option<EngineOperationAck>,acquire_clock:Option<FinalizationBudget>,renewal_clock:Option<FinalizationBudget>,finished:bool}
impl RegisteredEngineOwner {
    /// Derived only from this actual retained registered/config owner. No
    /// caller/environment toggle or downgrade after an unsupported V2 send.
    pub fn image_batch_transport(&self)->Result<bool>{self.assert_current()?;self.config.assert_current()?;Ok(self.config.data().image_preseed.as_ref().and_then(|p|p.batch_transport.as_deref())==Some("batch-v1"))}
    pub fn begin_operation_retaining(self:&Arc<Self>,out:&mut Option<EngineOperation>,operation:EngineOperationKind,command_key:String,original:FinalizationBudget,cancelled:Arc<AtomicBool>)->Result<()> {
        ensure!(!operation.image(),"image operation requires separate original image budget");
        self.begin_bounded_operation(out,operation,command_key,OperationBudget::Fresh(original),cancelled)
    }
    pub fn begin_image_operation_retaining(self:&Arc<Self>,out:&mut Option<EngineOperation>,operation:EngineOperationKind,command_key:String,original:ImagePreseedBudget,cancelled:Arc<AtomicBool>)->Result<()> {
        ensure!(operation.image(),"image budget cannot admit fresh/task operation");
        self.begin_bounded_operation(out,operation,command_key,OperationBudget::Image(original),cancelled)
    }
    /// Known contention leaves out empty. Caller retains the SAME original
    /// budget/key; a staged/sent/uncertain exchange is never recreated.
    pub fn try_begin_operation_retaining(self:&Arc<Self>,out:&mut Option<EngineOperation>,operation:EngineOperationKind,command_key:String,original:FinalizationBudget,cancelled:Arc<AtomicBool>)->Result<bool> {
        ensure!(!operation.image(),"image operation requires image budget");self.try_begin_bounded_operation(out,operation,command_key,OperationBudget::Fresh(original),cancelled)
    }
    pub fn try_begin_image_operation_retaining(self:&Arc<Self>,out:&mut Option<EngineOperation>,operation:EngineOperationKind,command_key:String,original:ImagePreseedBudget,cancelled:Arc<AtomicBool>)->Result<bool> {
        ensure!(operation.image(),"image budget cannot admit other operations");self.try_begin_bounded_operation(out,operation,command_key,OperationBudget::Image(original),cancelled)
    }
    pub fn try_begin_owner_renewal_retaining(self:&Arc<Self>,out:&mut Option<EngineOperation>,operation:EngineOperationKind,command_key:String,original:FinalizationBudget,cancelled:Arc<AtomicBool>)->Result<bool> {
        ensure!(matches!(operation,EngineOperationKind::RenewFreshOwnerV1 {..}),"renewal operation differs");self.try_begin_operation_retaining(out,operation,command_key,original,cancelled)
    }
    fn try_begin_bounded_operation(self:&Arc<Self>,out:&mut Option<EngineOperation>,operation:EngineOperationKind,command_key:String,original:OperationBudget,cancelled:Arc<AtomicBool>)->Result<bool> {
        ensure!(out.is_none(),"operation carrier occupied");self.assert_current()?;original.remaining()?;
        let mut channel=match self.operations.try_lock(){Ok(v)=>v,Err(TryLockError::WouldBlock)=>return Ok(false),Err(_)=>return Err(fault())};ensure!(!channel.fenced,"original operation channel fenced");let renewal=matches!(operation,EngineOperationKind::RenewFreshOwnerV1 {..});if channel.lane(renewal).is_some(){return Ok(false);}
        let nonce=uuid::Uuid::new_v4().to_string();let request=NativeRequest {version:1,request_nonce:nonce.clone(),command_key,operation,constraint:original.constraint()?};native_request(&request,self.config.data())?;let wire=bytes(&request)?;
        *out=Some(EngineOperation {registration:Arc::clone(self),nonce,original,cancelled,receipt:None,acquire_clock:None,renewal_clock:None,finished:false});
        *channel.lane(renewal)=Some(NativeExchange {request,wire,sent:false,received:None,ack:None,validated:false,uncertain:false});out.as_ref().unwrap().time()?;Ok(true)
    }
    fn begin_bounded_operation(self:&Arc<Self>,out:&mut Option<EngineOperation>,operation:EngineOperationKind,command_key:String,original:OperationBudget,cancelled:Arc<AtomicBool>)->Result<()> {
        ensure!(out.is_none(),"engine operation carrier already occupied");let nonce=uuid::Uuid::new_v4().to_string();
        *out=Some(EngineOperation {registration:Arc::clone(self),nonce:nonce.clone(),original,cancelled,receipt:None,acquire_clock:None,renewal_clock:None,finished:false});let held=out.as_ref().unwrap();held.time()?;self.assert_current()?;
        let request=NativeRequest {version:1,request_nonce:nonce,command_key,operation,constraint:held.original.constraint()?};native_request(&request,self.config.data())?;
        let wire=bytes(&request)?;let mut channel=self.operations.try_lock().map_err(|_|fault())?;ensure!(!channel.fenced,"operation channel fenced");let renewal=matches!(request.operation,EngineOperationKind::RenewFreshOwnerV1 {..});ensure!(channel.lane(renewal).is_none(),"engine operation lane has retained prior operation");
        *channel.lane(renewal)=Some(NativeExchange {request,wire,sent:false,received:None,ack:None,validated:false,uncertain:false});held.time()?;Ok(())
    }
}
impl EngineOperation {
    fn time(&self)->Result<()> {if self.cancelled.load(Ordering::Acquire){self.original.cancel();return Err(fault());}self.original.remaining()?;Ok(())}
    fn receive(channel:&mut NativeChannel,root_pid:u32,registration:&RegistrationAck)->Result<()> {
        let result=(||->Result<()>{if channel.unclassified.is_none(){channel.unclassified=channel.endpoint.receive()?;}let Some(packet)=&channel.unclassified else{return Ok(());};
            ensure!(packet.credentials.pid==root_pid as i32&&packet.credentials.uid==0&&packet.credentials.gid==0&&packet.bytes.len()<=OP_MAX,"operation reply is not original Root");
            let ack:EngineOperationAck=serde_json::from_slice(&packet.bytes).map_err(|_|fault())?;let nonce=ack.request_nonce.clone();let active=channel.get(&nonce).context("operation ACK has no exact original nonce")?;
            ensure!(active.received.is_none()&&(packet.fds.is_empty()||active.request.operation.begin_upload()&&packet.fds.len()==1),"operation ACK descriptor/duplicate differs");
            let packet=channel.unclassified.take().unwrap();let active=channel.get_mut(&nonce).unwrap();active.received=Some(packet);active.ack=Some(ack);
            let ack=active.ack.as_ref().unwrap();matches_registration(ack,registration)?;matches_native(ack,&active.request)?;
            ensure!(ack.error.is_none()||active.received.as_ref().unwrap().fds.is_empty(),"failed operation carried descriptor");active.validated=true;Ok(())})();if result.is_err(){channel.fenced=true;}result
    }
    pub fn poll_retaining(&mut self)->Result<bool> {self.time()?;self.registration.assert_current()?;
        let registered=Arc::clone(&self.registration);let mut channel=match registered.operations.try_lock(){Ok(v)=>v,Err(TryLockError::WouldBlock)=>return Ok(false),Err(_)=>return Err(fault())};
        ensure!(!channel.fenced&&channel.get(&self.nonce).is_some_and(|a|a.request.request_nonce==self.nonce)&&!self.finished,"engine operation original carrier differs");
        let result=(||->Result<bool>{let active=channel.get(&self.nonce).unwrap();ensure!(!active.uncertain,"engine operation uncertain; inspect original only");
            if !active.sent {if matches!(active.request.operation,EngineOperationKind::AcquireFreshOwnerV1 {..})&&self.acquire_clock.is_none(){
                self.acquire_clock=Some(FinalizationBudget::begin(std::time::Duration::from_secs(15))?);}
                if matches!(active.request.operation,EngineOperationKind::RenewFreshOwnerV1 {..})&&self.renewal_clock.is_none(){self.renewal_clock=Some(FinalizationBudget::begin(std::time::Duration::from_secs(15))?);}
                if let Some(clock)=&self.renewal_clock {clock.remaining()?;}
                if let Some(clock)=&self.acquire_clock {clock.remaining()?;}
                if !channel.endpoint.send(&active.wire,None)? {return Ok(false);}channel.get_mut(&self.nonce).unwrap().sent=true;self.time()?;}
            Self::receive(&mut channel,self.registration.root.pid,&self.registration.data)?;
            ensure!(channel.get(&self.nonce).unwrap().ack.is_none()||channel.get(&self.nonce).unwrap().validated,"original ACK validation incomplete");let Some(ack)=&channel.get(&self.nonce).unwrap().ack else {return Ok(false);};self.receipt=Some(ack.clone());self.time()?;if let Some(clock)=&self.acquire_clock {clock.remaining()?;}if let Some(clock)=&self.renewal_clock {clock.remaining()?;}self.registration.assert_current()?;Ok(true)})();
        if result.is_err(){channel.get_mut(&self.nonce).unwrap().uncertain=true;}result
    }
    /// Passive original-ACK observation after timeout/fence. Never resends or
    /// creates a live preparation/assignment owner from historical readback.
    pub fn reconcile_only_retaining(&mut self)->Result<bool> {let registered=Arc::clone(&self.registration);let mut channel=registered.operations.try_lock().map_err(|_|fault())?;
        ensure!(!channel.fenced&&channel.get(&self.nonce).is_some_and(|a|a.request.request_nonce==self.nonce),"engine operation original carrier absent");
        Self::receive(&mut channel,self.registration.root.pid,&self.registration.data)?;if let Some(ack)=&channel.get(&self.nonce).unwrap().ack {self.receipt=Some(ack.clone());return Ok(true);}Ok(false)}
    pub fn receipt_data(&self)->Option<&EngineOperationAck> {self.receipt.as_ref()}
    pub fn take_preparation_retaining(&mut self,out:&mut Option<Arc<FreshProjectPreparation>>)->Result<bool> {ensure!(out.is_none(),"fresh preparation output occupied");if !self.poll_retaining()?{return Ok(false);}
        let ack=self.receipt.as_ref().unwrap();ensure!(ack.error.is_none()&&matches!(ack.operation,EngineOperationKind::PrepareFreshProjectV1 {..}),"engine operation did not commit preparation");
        let data:FreshPreparationData=serde_json::from_value(ack.result.clone().unwrap()).map_err(|_|fault())?;
        let b=&self.registration.data.binding;data.prepare.validate()?;uuid(&data.project_id)?;ensure!(data.scope=="fresh-preparation-only"&&data.state=="preparation_reserved"&&data.account_id==b.account_id&&data.home_id==b.home_id&&data.store_id==b.store_id
            &&data.incarnation==b.incarnation&&data.engine_instance_id==self.registration.engine_id()&&data.boot_id==self.registration.boot_id()&&data.assignment_epoch=="0"&&data.workspace_id==ack.operation.workspace_id()
            &&data.project_key==format!("fresh-v4:{}",data.project_id),"fresh preparation does not bind original registration/namespace");
        let configured=self.registration.config.data().projects.iter().find(|p|p.alias==data.workspace_id).context("fresh preparation outside configured project")?;
        ensure!(data.prepare==configured.prepare&&data.prepare.namespace.project_id==data.project_id,"fresh preparation changed original configured namespace");
        *out=Some(Arc::new(FreshProjectPreparation {data,registration:Arc::clone(&self.registration),ack:ack.clone(),published:AtomicBool::new(false)}));
        self.time()?;self.registration.assert_current()?;self.finish_known_retaining()?;out.as_ref().unwrap().published.store(true,Ordering::Release);Ok(true)
    }
    pub fn take_upload_retaining(&mut self,out:&mut Option<RootUploadTransfer>)->Result<bool> {
        ensure!(out.is_none(),"upload output occupied");if !self.poll_retaining()?{return Ok(false);}let ack=self.receipt.as_ref().unwrap().clone();
        let scope=upload_scope(&ack,&self.registration.data)?;
        let registered=Arc::clone(&self.registration);let mut channel=registered.operations.try_lock().map_err(|_|fault())?;
        let active=channel.get_mut(&self.nonce).context("upload original exchange absent")?;
        ensure!(active.request.request_nonce==self.nonce&&active.validated,"upload original exchange differs");
        let packet=active.received.as_mut().context("upload original Root packet absent")?;ensure!(packet.fds.len()==1,"upload original Root stream absent");
        let fd=packet.fds.remove(0);
        RootUploadTransfer::from_original_retaining(out,Arc::clone(&self.registration),scope,self.original.clone(),Arc::clone(&self.cancelled),ack,fd)?;
        drop(channel);self.time()?;self.finish_known_retaining()?;Ok(true)
    }
    /// Current BeginImage ACK has exactly one disposition: original uploader
    /// FD, or a new authenticated reuse owner with zero FDs. Never reconstruct
    /// either owner from a saved receipt or a caller-selected boolean.
    pub fn take_image_begin_retaining(&mut self,upload:&mut Option<RootUploadTransfer>,published:&mut Option<Arc<PublishedImagePreseed>>)->Result<bool> {
        ensure!(upload.is_none()&&published.is_none(),"image Begin outputs occupied");
        if !self.poll_retaining()?{return Ok(false);}let ack=self.receipt.as_ref().unwrap().clone();
        ensure!(matches!(ack.operation,EngineOperationKind::BeginImagePreseedV1 {..}|EngineOperationKind::BeginImagePreseedV2 {..}),"operation is not image Begin");
        if !has_image_reuse_result(&ack){return self.take_upload_retaining(upload);}
        let _data=image_reuse_result(&ack,self.registration.config.data())?;
        {
            let registered=Arc::clone(&self.registration);let channel=registered.operations.try_lock().map_err(|_|fault())?;
            let active=channel.get(&self.nonce).context("image reuse original exchange absent")?;
            ensure!(active.validated&&active.request.request_nonce==self.nonce&&active.received.as_ref().is_some_and(|p|p.fds.is_empty()),"image reuse original peer/ACK/FD differs");
        }
        let (EngineOperationKind::BeginImagePreseedV1 {manifest,installer_input,image_receipt_sha256,..}|EngineOperationKind::BeginImagePreseedV2 {manifest,installer_input,image_receipt_sha256,..})=&ack.operation else {unreachable!()};
        *published=Some(Arc::new(PublishedImagePreseed {registration:Arc::clone(&self.registration),manifest:manifest.clone(),installer_input:installer_input.clone(),image_receipt_sha256:image_receipt_sha256.clone(),ack:ack.clone(),begin:ack.clone(),published:AtomicBool::new(false)}));
        self.time()?;self.registration.assert_current()?;self.finish_known_retaining()?;published.as_ref().unwrap().published.store(true,Ordering::Release);Ok(true)
    }
    pub fn take_image_publication_retaining(&mut self,transfer:&RootUploadTransfer,out:&mut Option<Arc<PublishedImagePreseed>>)->Result<bool> {
        ensure!(out.is_none(),"image publication output occupied");if !self.poll_retaining()?{return Ok(false);}transfer.assert_completed()?;
        ensure!(Arc::ptr_eq(&self.registration,transfer.registration()),"image publication registration differs");
        let ack=self.receipt.as_ref().unwrap();ensure!(ack.error.is_none()&&matches!(&ack.operation,EngineOperationKind::PublishImagePreseedV1 {workspace_id,transfer_id}
            if workspace_id==&transfer.scope_data().workspace_id&&transfer_id==&transfer.scope_data().transfer_id),"image publication original transfer differs");
        let (EngineOperationKind::BeginImagePreseedV1 {manifest,installer_input,image_receipt_sha256,..}|EngineOperationKind::BeginImagePreseedV2 {manifest,installer_input,image_receipt_sha256,..})=&transfer.begin_ack().operation else{return Err(fault());};
        #[derive(Deserialize)]#[serde(deny_unknown_fields)]struct ResultData {scope:String,state:String,transfer_id:String,project_id:String,workspace_id:String,installer_input:ObjectRef,manifest_publication:FabricRootPublication,publication:FabricRootPublication}
        let data:ResultData=serde_json::from_value(ack.result.clone().context("image publication result absent")?).map_err(|_|fault())?;
        ensure!(data.scope=="image-preseed-publication-only"&&data.state=="published"&&data.transfer_id==transfer.scope_data().transfer_id&&data.workspace_id==transfer.scope_data().workspace_id
            &&data.installer_input==*installer_input&&transfer.begin_ack().result.as_ref().and_then(|v|v.get("project_id")).and_then(|v|v.as_str())==Some(data.project_id.as_str()),"image publication result differs");uuid(&data.project_id)?;data.manifest_publication.matches(manifest)?;data.publication.matches(installer_input)?;
        *out=Some(Arc::new(PublishedImagePreseed {registration:Arc::clone(&self.registration),manifest:manifest.clone(),installer_input:installer_input.clone(),image_receipt_sha256:image_receipt_sha256.clone(),
            ack:ack.clone(),begin:transfer.begin_ack().clone(),published:AtomicBool::new(false)}));self.time()?;self.finish_known_retaining()?;out.as_ref().unwrap().published.store(true,Ordering::Release);Ok(true)
    }
    pub fn take_original_acquire_ack_retaining(&mut self,out:&mut Option<Arc<OriginalFreshAcquireAck>>)->Result<bool> {
        ensure!(out.is_none(),"fresh acquire output occupied");if !self.poll_retaining()?{return Ok(false);}let ack=self.receipt.as_ref().unwrap();
        let EngineOperationKind::AcquireFreshOwnerV1 {workspace_id,transfer_id,acquire_key,..}=&ack.operation else{return Err(fault());};ensure!(ack.error.is_none(),"fresh acquire rejected");
        #[derive(Deserialize)]#[serde(deny_unknown_fields)]struct ResultData{scope:String,state:String,transfer_id:String,project_id:String,workspace_id:String,acquire_key:String,owner_receipt:serde_json::Value}
        let d:ResultData=serde_json::from_value(ack.result.clone().context("fresh acquire result absent")?).map_err(|_|fault())?;
        ensure!(d.scope=="fresh-owner-acquire-only"&&d.state=="acquired"&&d.transfer_id==*transfer_id&&d.workspace_id==*workspace_id&&d.acquire_key==*acquire_key&&!d.owner_receipt.is_null(),"fresh acquire result differs");uuid(&d.project_id)?;
        let lease=self.acquire_clock.as_ref().context("fresh acquire original first-send clock absent")?.clone();
        *out=Some(Arc::new(OriginalFreshAcquireAck {registration:Arc::clone(&self.registration),ack:ack.clone(),lease,owner_receipt:d.owner_receipt,published:AtomicBool::new(false)}));
        self.time()?;out.as_ref().unwrap().lease.remaining()?;self.finish_known_retaining()?;out.as_ref().unwrap().published.store(true,Ordering::Release);Ok(true)
    }
    pub fn take_original_owner_renewal_ack_retaining(&mut self,out:&mut Option<Arc<OriginalOwnerRenewalAck>>)->Result<bool> {
        ensure!(out.is_none(),"original renewal output occupied");if !self.poll_retaining()?{return Ok(false);}let ack=self.receipt.as_ref().unwrap();
        let EngineOperationKind::RenewFreshOwnerV1 {workspace_id,request_id,..}=&ack.operation else{return Err(fault());};ensure!(ack.error.is_none(),"owner renewal rejected");
        #[derive(Deserialize)]#[serde(deny_unknown_fields)]struct Data {scope:String,state:String,project_id:String,workspace_id:String,request_id:String,owner_grant:serde_json::Value}
        let data:Data=serde_json::from_value(ack.result.clone().context("renewal result absent")?).map_err(|_|fault())?;
        let project=self.registration.config.data().projects.iter().find(|p|p.alias==*workspace_id).context("renewal project absent")?;
        ensure!(data.scope=="fresh-owner-renewal-only"&&data.state=="renewed"&&data.workspace_id==*workspace_id&&data.request_id==*request_id&&data.project_id==project.prepare.namespace.project_id&&!data.owner_grant.is_null(),"renewal original response differs");
        let lease=self.renewal_clock.as_ref().context("renewal original native first-send clock absent")?.clone();
        *out=Some(Arc::new(OriginalOwnerRenewalAck {registration:Arc::clone(&self.registration),ack:ack.clone(),lease,grant:data.owner_grant,published:AtomicBool::new(false)}));
        self.time()?;out.as_ref().unwrap().lease.remaining()?;self.registration.assert_current()?;self.finish_known_retaining()?;out.as_ref().unwrap().published.store(true,Ordering::Release);Ok(true)
    }
    /// Clears ONLY an actual validated known ACK. Caller still retains this
    /// receipt/registration; unknown or absent ACK never releases the slot.
    pub fn finish_known_retaining(&mut self)->Result<()> {ensure!(self.receipt.is_some()&&!self.finished,"engine operation original known ACK absent");
        let registered=Arc::clone(&self.registration);let mut channel=registered.operations.try_lock().map_err(|_|fault())?;ensure!(!channel.fenced&&channel.get(&self.nonce).is_some_and(|a|a.request.request_nonce==self.nonce&&a.validated),"engine operation slot differs");
        ensure!(channel.get(&self.nonce).unwrap().received.as_ref().is_none_or(|p|p.fds.is_empty()),"upload stream must move into retained output before finish");channel.take(&self.nonce);self.finished=true;Ok(())}
    // Enroll/status are authenticated DATA only. Historical OwnerGrant readback
    // must NEVER start/renew a15s lease at the time it is read. Actual original
    // acquire-send clock+ACK and server CAS/bootstrap producer are separate.
}
#[derive(Serialize)]
struct ProxyRelay<T> {kind:String,rpc_id:String,packet:T,transport:serde_json::Value}
#[derive(Deserialize)]#[serde(deny_unknown_fields)]
struct NeverForwarded {kind:String,rpc_id:String,request_sha256:String,transport:serde_json::Value}
struct RootExchange {native:NativeRequest,native_packet:storage::Packet,body:OperationBody,wire:Vec<u8>,proxy_target:Option<serde_json::Value>,never_forwarded:Vec<storage::Packet>,sent:bool,server:Option<storage::Packet>,ack:Option<Signed<EngineOperationAck>>,native_sent:bool,native_send_unknown:bool,uncertain:bool}
/// Exactly one regular/storage lane and one renewal lane. Same actual peer and
/// endpoint, independent original budgets, exact nonce demux; no effect replay.
pub(super) struct RootOperationBridge {upload:Option<RootUploader>,endpoint:storage::Endpoint,native:OwnedFd,pending:BTreeMap<String,RootExchange>,completed:std::collections::VecDeque<(String,String)>,unclassified:Option<storage::Packet>,inbound:Option<storage::Packet>,no_forward:Option<storage::Packet>,fenced:bool}
impl RootOperationBridge {
    pub(super) fn capture()->Result<Self>{let (root,native)=storage::bootstrap_socket_pair()?;Ok(Self {upload:None,endpoint:storage::Endpoint::capture(root)?,native,pending:BTreeMap::new(),completed:std::collections::VecDeque::new(),unclassified:None,inbound:None,no_forward:None,fenced:false})}
    fn record_completed(&mut self,p:&RootExchange)->Result<()>{let ack=p.ack.as_ref().context("known original ACK absent")?;self.completed.push_back((p.native.request_nonce.clone(),hex(&Sha256::digest(bytes(ack)?))));if self.completed.len()>64{self.completed.pop_front();}Ok(())}
    fn receive_never_forwarded(&mut self,host:&mut RootHostControl)->Result<()> {
        for _ in 0..2 {if self.no_forward.is_none(){host.take_unforwarded_retaining(&mut self.no_forward)?;}let Some(packet)=&self.no_forward else{break;};
            // Host IO already authenticated the actual original Node SCM peer.
            // This proof is emitted ONLY before any socket.send attempt. It
            // allows SAME-request dispatch, never retry of a network effect.
            let proof:NeverForwarded=serde_json::from_slice(&packet.bytes).map_err(|_|fault())?;
            let p=self.pending.get_mut(&proof.rpc_id).context("no-forward proof has no original operation")?;
            ensure!(proof.kind=="engine_operation_not_forwarded_v1"&&p.sent&&p.server.is_none()&&p.ack.is_none()
                &&p.proxy_target.as_ref()==Some(&proof.transport)&&proof.request_sha256==hex(&Sha256::digest(bytes(&p.body)?))&&p.never_forwarded.len()<8,"no-forward proof differs from original known dispatch");
            p.never_forwarded.push(self.no_forward.take().unwrap());p.sent=false;p.wire.clear();p.proxy_target=None;
        }Ok(())
    }
    fn receive_acks(&mut self,host:&mut RootHostControl,registration:&RegistrationAck,credential:&[u8],passive:bool)->Result<()> {
        for _ in 0..2 {if self.inbound.is_none(){if passive{host.operation_receive_settled_retaining(&mut self.inbound)?;}else{host.operation_receive_retaining(&mut self.inbound)?;}}
            let Some(server)=&self.inbound else{break;};let relay:Relay<Signed<EngineOperationAck>>=serde_json::from_slice(&server.bytes).map_err(|_|fault())?;
            ensure!(relay.kind=="engine_operation_ack_v1"&&relay.rpc_id==relay.packet.body.request_nonce,"engine operation ACK kind/nonce differs");verify(credential,ACK_DOMAIN,&relay.packet)?;matches_registration(&relay.packet.body,registration)?;
            let digest=hex(&Sha256::digest(bytes(&relay.packet)?));
            if !self.pending.contains_key(&relay.rpc_id){ensure!(self.completed.iter().any(|(nonce,hash)|nonce==&relay.rpc_id&&hash==&digest),"engine ACK has no original retained/completed nonce");self.inbound.take();continue;}
            let p=self.pending.get_mut(&relay.rpc_id).unwrap();ensure!(p.sent,"unissued original engine ACK retained");matches_signed(&relay.packet.body,&p.body,registration)?;
            if let Some(known)=&p.ack{ensure!(hex(&Sha256::digest(bytes(known)?))==digest,"duplicate original engine ACK differs");self.inbound.take();continue;}
            ensure!(p.server.is_none(),"original server packet awaiting validation");
            p.server=self.inbound.take();p.ack=Some(relay.packet);let ack=p.ack.as_ref().unwrap();verify(credential,ACK_DOMAIN,ack)?;matches_signed(&ack.body,&p.body,registration)?;
        }Ok(())
    }
    pub(super) fn reconcile_only_retaining(&mut self,host:&mut RootHostControl,registration:&RegistrationAck,credential:&[u8])->Result<bool> {
        ensure!(self.fenced||self.pending.values().any(|p|p.uncertain),"engine operation not uncertain");self.receive_acks(host,registration,credential,true)?;let mut observed=false;
        for p in self.pending.values_mut(){if let Some(ack)=&p.ack {observed=true;if !p.native_sent&&!p.native_send_unknown {match self.endpoint.send(&bytes(&ack.body)?,None){Ok(true)=>p.native_sent=true,Ok(false)=>{},Err(_)=>{p.native_send_unknown=true;return Err(fault());}}}}}Ok(observed)
    }
    pub(super) fn settle_uploaders_retaining(&mut self)->Result<()> {if let Some(upload)=&mut self.upload {upload.settle_retaining()?;}Ok(())}
    pub(super) fn original_pending_ack_bindings(&self)->Result<Vec<serde_json::Value>> {
        self.pending.values().filter(|p|p.sent&&p.ack.is_none()).map(|p|Ok(serde_json::json!({"request_nonce":p.native.request_nonce,"request_sha256":hex(&Sha256::digest(bytes(&p.body)?))}))).collect()
    }
    pub(super) fn native_fd(&self)->&OwnedFd {&self.native}
    pub(super) fn poll_retaining(&mut self,host:&mut RootHostControl,registration:&RegistrationAck,credential:&[u8],engine:&Process,config:&HostIngressConfig)->Result<()> {
        // A failed/slow data uploader quarantines its regular lane, not another
        // project's already-owned lease. Its actual child/FD owner stays held.
        if let Some(upload)=&mut self.upload {let _retained=upload.settle_retaining();}if self.fenced{return Ok(());}
        // The original Native may release its MESSAGE lane immediately after
        // a known ACK send, even if our post-send clock check then failed.
        // Clear only that already-delivered message before receiving its next
        // request; independent uploader/source/process owners remain held.
        let delivered:Vec<_>=self.pending.iter().filter(|(_,p)|p.native_sent).map(|(n,_)|n.clone()).collect();
        for nonce in delivered{let p=self.pending.remove(&nonce).unwrap();self.record_completed(&p)?;}
        let authenticated=(||->Result<()> {engine.live()?;
            for _ in 0..2 {if self.unclassified.is_none(){self.unclassified=self.endpoint.receive()?;}let Some(packet)=&self.unclassified else{break;};
                ensure!(packet.credentials.pid==engine.pid as i32&&packet.credentials.uid==1500&&packet.credentials.gid==1500&&packet.fds.is_empty()&&packet.bytes.len()<=OP_MAX,"engine operation peer is not original Native");
                let request:NativeRequest=serde_json::from_slice(&packet.bytes).map_err(|_|fault())?;native_scope(&request,config)?;let renewal=matches!(request.operation,EngineOperationKind::RenewFreshOwnerV1 {..});
                ensure!(!self.pending.values().any(|p|matches!(p.native.operation,EngineOperationKind::RenewFreshOwnerV1 {..})==renewal)&&self.pending.len()<2,"original engine lane occupied");
                ensure!(!request.operation.begin_upload()||self.upload.is_none(),"original uploader remains retained");let remaining=0;let b=&registration.binding;
                let body=OperationBody {version:1,kind:"engine_operation".into(),account_id:b.account_id.clone(),home_id:b.home_id.clone(),store_id:b.store_id.clone(),incarnation:b.incarnation.clone(),engine_instance_id:registration.engine_instance_id.clone(),boot_id:registration.boot_id.clone(),credential_id:registration.credential_id.clone(),epoch:b.epoch.clone(),root_nonce:registration.root_nonce.clone(),registration_proof_sha256:registration.proof_sha256.clone(),request_nonce:request.request_nonce.clone(),command_key:request.command_key.clone(),operation:request.operation.clone(),payload_sha256:hex(&Sha256::digest(bytes(&request.operation)?)),constraint:request.constraint.clone(),remaining_milliseconds:remaining};
                let wire=Vec::new();
                self.pending.insert(request.request_nonce.clone(),RootExchange {native:request,native_packet:self.unclassified.take().unwrap(),body,wire,proxy_target:None,never_forwarded:Vec::new(),sent:false,server:None,ack:None,native_sent:false,native_send_unknown:false,uncertain:false});
            }self.receive_never_forwarded(host)?;self.receive_acks(host,registration,credential,false)?;Ok(())})();if authenticated.is_err(){self.fenced=true;return authenticated;}
        let mut nonces:Vec<_>=self.pending.keys().cloned().collect();nonces.sort_by_key(|n|!matches!(self.pending[n].native.operation,EngineOperationKind::RenewFreshOwnerV1 {..}));
        for nonce in nonces {let p=self.pending.get_mut(&nonce).unwrap();
            let expired=p.native.constraint.adopt(p.native.operation.image()).and_then(|b|b.remaining()).is_err();
            if p.uncertain||expired {
                // Only the unchanged original MAC-validated ACK body may pass
                // after cutoff. No upload FD/owner/lease is created here. The
                // actual uploader/source/process owners remain quarantined.
                if let Some(ack)=&p.ack {if !p.native_sent&&!p.native_send_unknown {match self.endpoint.send(&bytes(&ack.body)?,None){Ok(true)=>p.native_sent=true,Ok(false)=>{},Err(_)=>p.native_send_unknown=true,}}}
                if p.native_sent{let p=self.pending.remove(&nonce).unwrap();self.record_completed(&p)?;}continue;
            }
            let progress=(||->Result<bool>{let budget=p.native.constraint.adopt(p.native.operation.image())?;budget.remaining()?;
            if !p.sent {if let EngineOperationKind::PublishFreshRootsV1 {workspace_id,transfer_id}|EngineOperationKind::PublishImagePreseedV1 {workspace_id,transfer_id}=&p.native.operation {
                let upload=self.upload.as_mut().context("publication original uploader absent")?;ensure!(upload.scope().workspace_id==*workspace_id&&upload.scope().transfer_id==*transfer_id&&(if p.native.operation.image(){matches!(upload.scope().mode.as_str(),"image"|"image_batch_v1")}else{upload.scope().mode=="fresh"}),"publication uploader differs");if !upload.successful()?{return Ok(false);}}
                let Some(target)=host.original_proxy_target()? else{return Ok(false);};
                p.body.remaining_milliseconds=u32::try_from(budget.remaining()?.as_millis().min(if p.native.operation.image(){300000}else{30000}))?;ensure!(p.body.remaining_milliseconds>0,"operation original budget lost");let signed=Signed {mac:sign(credential,REQUEST_DOMAIN,&p.body)?,body:p.body.clone()};p.proxy_target=Some(target.clone());p.wire=bytes(&ProxyRelay {kind:"engine_operation_v1".into(),rpc_id:p.native.request_nonce.clone(),packet:signed,transport:target})?;
                match host.registration_send(&p.wire){Ok(true)=>p.sent=true,Ok(false)=>return Ok(false),Err(error)=>{p.sent=true;return Err(error);}}budget.remaining()?;}
            let Some(ack)=&p.ack else{return Ok(false);};budget.remaining()?;
            let reused=ack.body.error.is_none()&&has_image_reuse_result(&ack.body);
            if reused {image_reuse_result(&ack.body,config)?;ensure!(self.upload.is_none(),"image reuse cannot replace an original uploader");}
            if !reused&&ack.body.error.is_none()&&p.native.operation.begin_upload()&&self.upload.is_none(){let scope=upload_scope(&ack.body,registration)?;let configured=config.projects.iter().find(|v|v.alias==scope.workspace_id).context("upload configured project absent")?;let result:BeginUploadResult=serde_json::from_value(ack.body.result.clone().unwrap()).map_err(|_|fault())?;ensure!(result.project_id==configured.prepare.namespace.project_id,"upload project differs");RootUploader::prepare_retaining(&mut self.upload,host,scope,budget.clone())?;}
            let fd=if !reused&&ack.body.error.is_none()&&p.native.operation.begin_upload(){let upload=self.upload.as_ref().context("original upload absent")?;ensure!(upload.scope().request_nonce==p.native.request_nonce,"upload nonce differs");Some(upload.native_fd()?)}else{None};
            match self.endpoint.send(&bytes(&ack.body)?,fd){Ok(true)=>{p.native_sent=true;if !reused&&p.native.operation.begin_upload()&&ack.body.error.is_none(){self.upload.as_mut().unwrap().delivered();}budget.remaining()?;Ok(true)},Ok(false)=>Ok(false),Err(_)=>{p.native_send_unknown=true;Err(fault())}}
        })();match progress {Ok(true)=>{let p=self.pending.remove(&nonce).unwrap();self.record_completed(&p)?;if p.ack.as_ref().is_some_and(|a|a.body.error.is_none())&&matches!(p.native.operation,EngineOperationKind::PublishFreshRootsV1 {..}|EngineOperationKind::PublishImagePreseedV1 {..}){self.upload.take();}},Ok(false)=>{},Err(_)=>{self.pending.get_mut(&nonce).unwrap().uncertain=true;}}}
        engine.live()?;Ok(())
    }
}
#[cfg(test)]
#[path="engine_operations_tests.rs"]
mod tests;

#[cfg(test)]
#[path="fresh_root_uploader_tests.rs"]
mod uploader_tests;

#[cfg(test)]
#[path="owner_renewal_mux_tests.rs"]
mod owner_renewal_mux_tests;

#[cfg(test)]
#[path="image_publication_reuse_tests.rs"]
mod image_reuse_tests;

#[cfg(test)]#[path="image_batch_operation_tests.rs"]mod image_batch_operation_tests;
