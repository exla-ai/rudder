//! Actual original Root CoW RPC custody. Never recreated from seed/path/JSON.
use super::*;
use rudder_vcs_bridge::{AttemptBinding,seed::{SeedProfile,CowMetadataForkQuiesced,paged::*}};
#[derive(Clone,Copy,PartialEq,Eq)]
enum CowRpcKind{Create,Archive}
struct CowRpc{kind:CowRpcKind,cancelled:Arc<AtomicBool>,cancel_event:OwnedFd,serial:u64,bytes:Vec<u8>,original:FinalizationBudget,sent:bool,packet:Option<service::Packet>,reply:Option<Reply>}
/// Keeps the actual original Root messages and private mount/ACK through every
/// error, panic, missed delivery and late result. No Drop cleanup or retry.
pub struct ClientMetadataCowOwner {
    instance:uuid::Uuid,slot:uuid::Uuid,seed:ObjectRef,manifest:ObjectRef,binding:AttemptBinding,nonce:String,
    original:FinalizationBudget,cancelled:Arc<AtomicBool>,start:SeedTaskUseGate,unspent_vcs:Option<CowVcsNeverCalled>,archive_evidence:Option<CowArchiveEvidence>,operations:Vec<CowRpc>,ready:Option<(PathBuf,ObjectRef)>,
    helper:Option<uuid::Uuid>,archive_request:Option<String>,archive_original:Option<(FinalizationBudget,Arc<AtomicBool>)>,archive:Option<Arc<ArchivedMetadataCow>>,
}
/// Original archive ACK + actual VCS quiescence, not a task/source grant.
/// Root retains the independently pinned private checkpoint and original
/// mount/server/TLS owner after releasing the active seed slot.
pub struct ArchivedMetadataCow {
    instance:uuid::Uuid,slot:uuid::Uuid,nonce:String,request:String,seed:ObjectRef,binding:AttemptBinding,
    directory:ObjectRef,hardlinks:ObjectRef,_quiescence:CowArchiveEvidence,
}
impl ArchivedMetadataCow {
    pub fn binding(&self)->&AttemptBinding{&self.binding}
    pub fn seed(&self)->&ObjectRef{&self.seed}
    pub fn directory(&self)->&ObjectRef{&self.directory}
    pub fn hardlinks(&self)->&ObjectRef{&self.hardlinks}
    pub(crate) fn retained_fact(&self)->Result<()>{ensure!(self.binding.seed==self.seed&&!self.nonce.is_empty()&&!self.request.is_empty(),"original CoW archive binding differs");Ok(())}
}
impl ClientMetadataCowOwner {
    pub fn binding(&self)->&AttemptBinding{&self.binding}
    pub fn nonce(&self)->&str{&self.nonce}
    pub fn archive(&self)->Option<&Arc<ArchivedMetadataCow>>{self.archive.as_ref()}
    pub fn archive_input_retained(&self)->bool{self.archive_evidence.is_some()}
    pub fn archive_request_started(&self)->bool{self.operations.iter().any(|o|o.kind==CowRpcKind::Archive)}
    pub fn original_mount(&mut self,original:&FinalizationBudget)->Result<(&Path,&str,&ObjectRef)>{
        ensure!(original.constraint()==self.original.constraint(),"CoW original preparation budget replaced");original.remaining()?;
        let(root,base)=self.ready.as_ref().context("original Root CoW ready ACK absent")?;
        ensure!(!self.cancelled.load(Ordering::Acquire),"original CoW factory cancelled");
        self.unspent_vcs.take().context("original CoW factory-use permit already consumed")?;Ok((root,&self.nonce,base))
    }
}
impl RootStorageClient {
    pub fn create_metadata_cow_retaining(&mut self,out:&mut Option<ClientMetadataCowOwner>,seed:&ClientSeedOwner,
        template:&VerifiedMetadataTemplate,binding:AttemptBinding,start:SeedTaskUseGate,original:&FinalizationBudget,cancelled:Arc<AtomicBool>)->Result<()> {
        ensure!(out.is_none()&&seed.profile==SeedProfile::PagedCowV2&&template.profile()==seed.profile,"CoW original owner/template profile differs");
        original.remaining()?;let slot=self.original_slot(seed)?;seed.uses.admit()?;
        ensure!(template.retained_seed()==Some(seed.seed())&&binding.seed==*seed.seed(),"CoW original source/seed differs");
        seed.uses.bind_cow_start(&start,&binding)?;
        *out=Some(ClientMetadataCowOwner {instance:self.instance.context("actual Root service absent")?,slot,seed:seed.seed().clone(),manifest:template.identity().clone(),binding,
            nonce:uuid::Uuid::new_v4().to_string(),original:original.clone(),cancelled:cancelled.clone(),start,unspent_vcs:Some(CowVcsNeverCalled{_private:()}),archive_evidence:None,operations:Vec::with_capacity(3),ready:None,helper:None,archive_request:None,archive_original:None,archive:None});
        let owner=out.as_mut().unwrap();let request=Request::CreateMetadataCow{slot,nonce:owner.nonce.clone(),binding:owner.binding.clone()};
        self.cow_rpc_retaining(owner,request,CowRpcKind::Create,original,cancelled)?;
        let reply=owner.operations.last().unwrap().reply.as_ref().context("original CoW reply absent")?;
        if let Reply::MetadataCowReady{root,base,..}=reply {owner.ready=Some((root.clone(),base.clone()));}
        else{anyhow::bail!("original CoW Root acknowledgement invalid; carrier retained")}
        // The original result is staged ABOVE before fallible validation.
        let Reply::MetadataCowReady{slot:actual,nonce,root,base}=reply else{unreachable!()};
        ensure!(*actual==slot&&nonce==&owner.nonce&&root.is_absolute()&&root.starts_with("/run/rudder-managed-seeds")
            &&root.file_name().is_some_and(|n|n=="private")&&seed.cow_floor.as_ref()==Some(base),"original CoW Root ACK binding differs");
        self.current()?;seed.source.check()?;original.remaining()?;self.private_v2_recovery=None;Ok(())
    }
    pub fn prepare_metadata_cow_enrollment_retaining(&self,owner:&ClientMetadataCowOwner,seed:&mut ClientSeedOwner,original:&FinalizationBudget)->Result<()> {
        self.original_slot(seed)?;original.remaining()?;
        ensure!(owner.ready.is_some()&&owner.helper.is_none()&&owner.archive.is_none()&&owner.instance==self.instance.unwrap()&&seed.slot==Some(owner.slot)
            &&seed.enrollment_cow.is_none(),"CoW original enrollment context differs");seed.enrollment_cow=Some(owner.nonce.clone());Ok(())
    }
    /// Pure bookkeeping after the SAME atomic Root EnrollHelperCow ACK. No
    /// second request or inferred helper association occurs.
    pub fn bind_metadata_cow_helper_retaining(&mut self,owner:&mut ClientMetadataCowOwner,seed:&mut ClientSeedOwner,helper:&EnrolledSeedHelper,
        original:&FinalizationBudget)->Result<()> {
        self.original_slot(seed)?;ensure!(owner.instance==helper.instance&&owner.slot==helper.slot&&seed.helpers.contains_key(&helper.reader)
            &&owner.helper.is_none()&&owner.ready.is_some()&&seed.enrollment_cow.as_deref()==Some(owner.nonce.as_str()),"CoW original helper/seed differs");
        owner.helper=Some(helper.reader);seed.enrollment_cow=None;original.remaining()?;self.private_v2_recovery=None;Ok(())
    }
    pub fn archive_metadata_cow_retaining(&mut self,owner:&mut ClientMetadataCowOwner,seed:&ClientSeedOwner,
        proof:&mut Option<CowMetadataForkQuiesced>,original:&FinalizationBudget,cancelled:&Arc<AtomicBool>)->Result<()> {
        if owner.archive.is_some(){return Ok(())}self.original_slot(seed)?;original.remaining()?;
        if owner.archive_evidence.is_none(){let actual=proof.as_ref().context("actual original CoW quiescence absent")?;actual.check_current(original,cancelled)?;
            ensure!(actual.binding()==&owner.binding&&actual.mount_nonce()==owner.nonce&&actual.seed_manifest()==&owner.manifest
                &&owner.seed==*seed.seed(),"CoW original quiescence/seed differs");
            owner.archive_evidence=Some(CowArchiveEvidence::Vcs(proof.take().unwrap()));
        }
        self.archive_cow_effect_retaining(owner,original,cancelled)
    }
    /// The actual native NeverStarted capsule separately proves preparation/
    /// broker/TLS joins. This original positive permit proves NO mount path was
    /// handed to VCS; the exact seal proves helper/enrollment never attempted.
    pub fn archive_unadopted_cow_retaining(&mut self,owner:&mut ClientMetadataCowOwner,seed:&ClientSeedOwner,seal:&SealedSeedTaskUse,
        original:&FinalizationBudget,cancelled:&Arc<AtomicBool>)->Result<()> {
        if owner.archive.is_some(){return Ok(())}self.original_slot(seed)?;seal.check_original(&owner.start)?;
        ensure!(owner.seed==*seed.seed()&&owner.ready.is_some(),"unadopted CoW original ready fact absent");
        if owner.archive_evidence.is_none(){let permit=owner.unspent_vcs.take().context("original VCS-use permit consumed; actual VCS quiescence required")?;
            owner.archive_evidence=Some(CowArchiveEvidence::Unadopted{_permit:permit,_seal:seal.clone()});}
        self.archive_cow_effect_retaining(owner,original,cancelled)
    }
    fn archive_cow_effect_retaining(&mut self,owner:&mut ClientMetadataCowOwner,original:&FinalizationBudget,cancelled:&Arc<AtomicBool>)->Result<()> {
        if let Some((prior,actual))=&owner.archive_original{ensure!(prior.constraint()==original.constraint()&&Arc::ptr_eq(actual,cancelled),"CoW original archive clock/cancel substituted");}
        else{owner.archive_original=Some((original.clone(),cancelled.clone()));}
        let request=owner.archive_request.get_or_insert_with(||uuid::Uuid::new_v4().to_string()).clone();
        self.cow_rpc_retaining(owner,Request::ArchiveMetadataCow{slot:owner.slot,nonce:owner.nonce.clone(),request},CowRpcKind::Archive,original,cancelled.clone())?;
        self.stage_original_cow_archive_fact(owner)?;
        original.remaining()?;ensure!(!cancelled.load(Ordering::Acquire),"CoW archive late cancellation");Ok(())
    }
    fn stage_original_cow_archive_fact(&self,owner:&mut ClientMetadataCowOwner)->Result<()> {
        if owner.archive.is_some(){return Ok(())}let reply=owner.operations.last().and_then(|o|o.reply.as_ref()).context("CoW original archive reply absent")?;
        let Reply::MetadataCowArchived{slot,nonce,request,directory,hardlinks}=reply else{anyhow::bail!("CoW original archive ACK absent")};
        ensure!(*slot==owner.slot&&nonce==&owner.nonce&&owner.archive_request.as_ref()==Some(request),"CoW original archive ACK differs");
        owner.archive=Some(Arc::new(ArchivedMetadataCow{instance:owner.instance,slot:owner.slot,nonce:nonce.clone(),request:request.clone(),seed:owner.seed.clone(),binding:owner.binding.clone(),
            directory:directory.clone(),hardlinks:hardlinks.clone(),_quiescence:owner.archive_evidence.take().context("original CoW archive input absent")?}));
        owner.archive.as_ref().unwrap().retained_fact()
    }
    /// No send/new cutoff/readback. Adopt only the ORIGINAL late response,
    /// with its SAME already-retained actual quiescence/unused permit.
    pub fn reconcile_cow_archive_fact_retaining(&mut self,owner:&mut ClientMetadataCowOwner)->Result<bool>{
        if owner.archive.is_some(){return Ok(true)}self.current_physical()?;
        ensure!(owner.operations.last().is_some_and(|r|r.kind==CowRpcKind::Archive),"original archive request absent");
        if owner.operations.last().unwrap().reply.is_none()&&!self.observe_original_cow_reply_retaining(owner)?{return Ok(false)}
        self.stage_original_cow_archive_fact(owner)?;Ok(true)
    }
    fn cow_rpc_retaining(&mut self,owner:&mut ClientMetadataCowOwner,request:Request,kind:CowRpcKind,original:&FinalizationBudget,cancelled:Arc<AtomicBool>)->Result<()> {
        if kind==CowRpcKind::Archive{self.current_physical()?;}else{self.current()?;}original.remaining()?;
        self.private_v2_recovery=Some(if kind==CowRpcKind::Archive{RootPrivateEffect::CowArchive}else{RootPrivateEffect::CowPreparation});
        ensure!(self.retirement_inflight.is_none()&&self.cow_inflight.is_none()&&self.seed_window_inflight.is_none()&&self.generic_inflight.is_none()&&owner.operations.len()<3
            &&!owner.operations.iter().any(|o|o.kind==kind),"original CoW RPC unresolved/already attempted");
        let serial=self.serial.checked_add(1).context("CoW serial exhausted")?;
        let bytes=service::encode(&Envelope{version:1,serial,constraint:original.constraint(),request},&[])?;
        let cancel_event=service::eventfd()?;
        owner.operations.push(CowRpc{kind,cancelled:cancelled.clone(),cancel_event,serial,bytes,original:original.clone(),sent:false,packet:None,reply:None});
        self.serial=serial;self.cow_inflight=Some(owner.nonce.clone());
        let result=(||{
            loop{if kind==CowRpcKind::Archive{self.current_physical()?;}else{self.current()?;}cow_time(owner,original)?;let rpc=owner.operations.last_mut().unwrap();
                if self.endpoint.send(&rpc.bytes,Some(&rpc.cancel_event))?{rpc.sent=true;break}std::thread::sleep(original.phase_limit(Duration::from_millis(2))?);}
            loop{if kind==CowRpcKind::Archive{self.current_physical()?;}else{self.current()?;}cow_time(owner,original)?;
                if self.observe_original_cow_reply_retaining(owner)?{original.remaining()?;return Ok(())}
                std::thread::sleep(original.phase_limit(Duration::from_millis(2))?);}
        })();if result.is_err(){self.fenced=true;}result
    }
    /// At most one receive, NO send/new budget/replay. Stages the actual packet
    /// before decoding, including malformed/foreign outcomes. A late known ACK
    /// is DATA until its original appropriate owner method validates it.
    pub fn observe_original_cow_reply_retaining(&mut self,owner:&mut ClientMetadataCowOwner)->Result<bool>{
        self.current_physical()?;ensure!(self.cow_inflight.as_deref()==Some(&owner.nonce),"another original CoW RPC is pending");
        let rpc=owner.operations.last_mut().context("original CoW RPC absent")?;
        if rpc.packet.is_none(){rpc.packet=self.endpoint.receive()?;}
        let Some(packet)=&rpc.packet else{return Ok(false)};
        let retained=self.private_v2_recovery.take();
        ensure!(packet.fds.is_empty()&&packet.credentials.pid==self.server.pid as i32&&packet.credentials.uid==0&&packet.credentials.gid==0,"CoW reply is not original authenticated Root");
        let(answer,payload):(Answer,_)=service::decode(&packet.bytes)?;
        ensure!(answer.version==1&&answer.serial==rpc.serial&&payload.is_empty(),"CoW original reply order differs");
        rpc.reply=Some(answer.reply);self.cow_inflight=None;self.private_v2_recovery=retained;Ok(true)
    }
}

impl RootStorageClient {
    pub fn acknowledge_cow_fork_retaining(&mut self,seed:&ClientSeedOwner,out:&mut Option<crate::root_storage_uses::RetainedCowFork>,
        task:&SeedUseGuard,fork:&SeedUseGuard,cow:&ClientMetadataCowOwner,seal:Option<&SealedSeedTaskUse>,original:&FinalizationBudget,cancelled:&AtomicBool)->Result<()> {
        self.current_physical()?;seed.source.check()?;
        ensure!(cow.instance==self.instance.context("original Root service absent")?&&seed.instance==Some(cow.instance)&&seed.slot==Some(cow.slot),"original CoW ACK belongs to another Root owner");
        seed.uses.acknowledge_cow_retaining(out,task,fork,cow.archive.as_ref().context("original CoW archive ACK absent")?,seal,original,cancelled)
    }
}

impl RootStorageClient {
    pub fn acknowledge_unused_cow_fork_retaining(&mut self,seed:&ClientSeedOwner,out:&mut Option<RetainedUnusedFork>,task:&SeedUseGuard,
        fork:&SeedUseGuard,cow:&ClientMetadataCowOwner,seal:&SealedSeedTaskUse,original:&FinalizationBudget,cancelled:&AtomicBool)->Result<()> {
        self.current_physical()?;seed.source.check()?;
        ensure!(cow.instance==self.instance.context("original Root service absent")?&&seed.instance==Some(cow.instance)&&seed.slot==Some(cow.slot),"original unused CoW belongs to another seed");
        seed.uses.acknowledge_unused_cow_retaining(out,task,fork,cow.archive.as_ref().context("original unused CoW archive ACK absent")?,seal,original,cancelled)
    }
}

fn cow_time(owner:&ClientMetadataCowOwner,original:&FinalizationBudget)->Result<()> {
    let rpc=owner.operations.last().context("original CoW RPC absent")?;
    if rpc.cancelled.load(Ordering::Acquire){service::cancel_event(&rpc.cancel_event);original.cancel();anyhow::bail!("original CoW operation cancelled");}original.remaining()?;Ok(())
}

struct CowVcsNeverCalled{_private:()}
enum CowArchiveEvidence {Vcs(CowMetadataForkQuiesced),Unadopted{_permit:CowVcsNeverCalled,_seal:SealedSeedTaskUse}}

impl RootStorageClient {
    /// Original late Create ACK is retained only as a resource fact. It never
    /// renews original_mount()'s task cutoff or opens task/start admission.
    pub fn reconcile_cow_ready_fact_retaining(&mut self,owner:&mut ClientMetadataCowOwner)->Result<bool>{
        self.current_physical()?;if owner.ready.is_some(){return Ok(true)}
        ensure!(owner.operations.last().is_some_and(|r|r.kind==CowRpcKind::Create),"original CoW creation request absent");
        if owner.operations.last().unwrap().reply.is_none()&&!self.observe_original_cow_reply_retaining(owner)?{return Ok(false)}
        let Reply::MetadataCowReady{slot,nonce,root,base}=owner.operations.last().unwrap().reply.as_ref().context("original CoW reply absent")? else{anyhow::bail!("original CoW creation not acknowledged")};
        ensure!(*slot==owner.slot&&nonce==&owner.nonce,"original CoW resource ACK differs");owner.ready=Some((root.clone(),base.clone()));Ok(true)
    }
}
