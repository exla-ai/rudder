//! Actual original runtime client -> original Root service -> original baked
//! descriptors. This role is runtime DATA backing, never compiler SDK, source,
//! project, lease, helper activation or retirement authority.
use super::*;
use rudder_engine::{protocol::ObjectRef,
    store::backed_inputs::{VerifiedBakedInputs, VerifiedBakedRead},
    directory::{DirectoryStore, PagedDirectory, EntryNode}};
use std::{cell::RefCell,thread};
const REQUEST_KIND: &str = "runtime_environment_backing_v1";
const ACK_KIND: &str = "runtime_environment_backing_ack_v1";
#[derive(Debug)]pub struct RuntimeBackingBusy;
impl std::fmt::Display for RuntimeBackingBusy{fn fmt(&self,f:&mut std::fmt::Formatter<'_>)->std::fmt::Result{f.write_str("original runtime backing custody temporarily borrowed")}}
impl std::error::Error for RuntimeBackingBusy{}
// Compatibility with the separately versioned restrictive qualification DATA
// field. This check only REFUSES work; it cannot construct any owner/grant.
fn environment_allowed(config:&HostIngressConfig)->Result<()> {
    reject_qualification_data(&serde_json::to_value(config)?)
}
fn reject_qualification_data(data:&serde_json::Value)->Result<()> {
    ensure!(data.get("runtime_qualification").is_none_or(serde_json::Value::is_null),
        "runtime qualification scope forbids environment/backing/source preparation");Ok(())
}

/// These originals are captured by the actual Root floor owner BEFORE its
/// BakedImageCas construction. Export never reopens a path or invents an FD.
pub(super) struct RootBackingDescriptors { root:Option<File>, inventory:Option<File>,
    stamps:Option<[(u64,u64,u64,u32,i64,i64);2]> }
impl RootBackingDescriptors {
    pub(super) fn capture_retaining(out:&mut Option<Self>)->Result<()> {
        ensure!(out.is_none(),"original runtime backing descriptors already staged");
        *out=Some(Self{root:None,inventory:None,stamps:None});let s=out.as_mut().unwrap();
        s.root=Some(protected_directory(Path::new(BAKED_IMAGE_DIRECTORY))?);
        let parent=s.root.as_ref().unwrap();let name=CString::new("inventory.json")?;
        let fd=unsafe{libc::openat(parent.as_raw_fd(),name.as_ptr(),libc::O_RDONLY|libc::O_NOFOLLOW|libc::O_CLOEXEC|libc::O_NONBLOCK)};
        ensure!(fd>=0,"original runtime backing inventory open failed");
        s.inventory=Some(unsafe{File::from_raw_fd(fd)});
        s.stamps=Some([stamp(s.root.as_ref().unwrap())?,stamp(s.inventory.as_ref().unwrap())?]);s.current()
    }
    fn current(&self)->Result<()> {
        let expected=self.stamps.as_ref().context("original runtime backing descriptor capture incomplete")?;
        for (i,f) in [self.root.as_ref().unwrap(),self.inventory.as_ref().unwrap()].into_iter().enumerate(){
            let m=f.metadata()?;ensure!(m.uid()==0&&m.gid()==0&&m.mode()&0o022==0&&stamp(f)?==expected[i]
                &&unsafe{libc::fcntl(f.as_raw_fd(),libc::F_GETFL)}&libc::O_ACCMODE==libc::O_RDONLY,
                "original runtime backing descriptor changed");
        }Ok(())
    }
    pub(super) fn duplicate_for_baked(&self)->Result<(OwnedFd,OwnedFd)> {
        self.current()?;let pair=(self.root.as_ref().unwrap().try_clone()?.into(),self.inventory.as_ref().unwrap().try_clone()?.into());
        self.current()?;Ok(pair)
    }
}

#[derive(Clone,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {version:u32,kind:String,nonce:String,engine:String,epoch:String,config_sha256:String,
    registration_proof_sha256:String,inventory:ObjectRef,constraint:FinalizationConstraint}
#[derive(Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Ack {version:u32,kind:String,nonce:String,engine:String,epoch:String,config_sha256:String,
    registration_proof_sha256:String,boot_id:String,inventory:ObjectRef,manifest:ObjectRef,
    installer_input:ObjectRef,mount_id:u64,selected_executables:Vec<RuntimeExecutableData>}

pub(super) fn is_request(bytes:&[u8])->Result<bool>{
    let data:serde_json::Value=serde_json::from_slice(bytes)?;
    Ok(data.get("kind").and_then(serde_json::Value::as_str)==Some(REQUEST_KIND))
}
/// The actual packet, originals and unknown send outcome remain in the Root
/// service. One export per machine floor, no normal retirement/reuse API.
pub(super) struct RootBackingTransfer {packet:RuntimePacket,request:Option<Request>,original:Option<FinalizationBudget>,
    bytes:Option<Vec<u8>>,fds:Vec<OwnedFd>,preparing:bool,sent:bool,failed:bool,failure:Option<anyhow::Error>}
impl RootBackingTransfer {
    pub(super) fn stage(packet:RuntimePacket)->Self{Self{packet,request:None,original:None,bytes:None,fds:Vec::new(),preparing:false,sent:false,failed:false,failure:None}}
    pub(super) fn poll_retaining(&mut self,owner:&RuntimeImageMountOwner,endpoint:&RuntimeEndpoint)->Result<bool>{
        if self.sent||self.failed{return Ok(false)}
        match self.poll_inner(owner,endpoint){Ok(v)=>Ok(v),Err(e)=>{self.failed=true;self.failure=Some(e);anyhow::bail!("original runtime backing transfer failed; retain packet/FDs")}}
    }
    fn poll_inner(&mut self,owner:&RuntimeImageMountOwner,endpoint:&RuntimeEndpoint)->Result<bool>{
        owner.current()?;
        environment_allowed(owner.config.as_ref().context("actual runtime protected config absent")?)?;
        if self.request.is_none(){
            ensure!(self.packet.fds.len()==1,"runtime backing request cancellation descriptor set differs");
            let request:Request=serde_json::from_slice(&self.packet.bytes)?;
            self.original=Some(FinalizationBudget::from_constraint(&request.constraint)?);self.request=Some(request);
        }
        let original=self.original.as_ref().unwrap();original.remaining()?;check_cancel(&self.packet.fds[0])?;
        let request=self.request.as_ref().unwrap();let a=owner.admission.as_ref().context("actual runtime read owner absent")?;
        let binding=&a.registration.ack;let baked=owner.baked.as_ref().context("actual runtime baked owner absent")?;
        ensure!(request.version==1&&request.kind==REQUEST_KIND&&uuid::Uuid::parse_str(&request.nonce).is_ok_and(|u|u.get_version_num()==4&&u.to_string()==request.nonce)
            &&request.engine==binding.engine_instance_id&&request.epoch==binding.binding.epoch&&request.config_sha256==a.config_sha
            &&request.registration_proof_sha256==binding.proof_sha256&&request.inventory==*baked.inventory_reference_data(),
            "runtime backing original registration/config/catalog binding differs");
        if self.bytes.is_none(){
            ensure!(!self.preparing,"original runtime backing preparation interrupted; no replay");self.preparing=true;
            let inventory=baked.inventory_data()?;let actual=owner.original_backing.as_ref().context("original baked descriptor owner absent")?;
            actual.current()?;let (root,index)=actual.duplicate_for_baked()?;self.fds.push(root);self.fds.push(index);
            self.bytes=Some(canonical_json(&Ack{version:1,kind:ACK_KIND.into(),nonce:request.nonce.clone(),engine:request.engine.clone(),epoch:request.epoch.clone(),
                config_sha256:a.config_sha.clone(),registration_proof_sha256:binding.proof_sha256.clone(),boot_id:binding.boot_id.clone(),
                inventory:baked.inventory_reference_data().clone(),manifest:inventory.roots.manifest,installer_input:inventory.roots.installer_input,
                mount_id:wire::fd_data(owner.root_fd.as_ref().unwrap())?.mount_id,selected_executables:owner.selected.clone()})?);
            self.preparing=false;
        }
        owner.current()?;original.remaining()?;check_cancel(&self.packet.fds[0])?;
        let refs=self.fds.iter().collect::<Vec<_>>();
        if !endpoint.send_data(self.bytes.as_ref().unwrap(),&refs)?{return Ok(false)}
        // Keep transfer duplicates too: normal runtime actor/read drain is not
        // available. A send ACK is not Native validation or environment ready.
        self.sent=true;original.remaining()?;owner.current()?;Ok(true)
    }
}

struct RequestFence {original:FinalizationBudget,cancelled:Arc<AtomicBool>}
impl ImageImportFence for RequestFence {fn remaining(&self)->Result<Duration>{
    if self.cancelled.load(Ordering::Acquire){self.original.cancel();}self.original.remaining()}}
#[derive(Clone,Debug)]
pub struct RuntimeToolData {pub name:String,pub installer_path:String,pub content:ObjectRef,pub sha256:String,pub size_bytes:u64}

pub(super) struct NativeBackingState {original:FinalizationBudget,cancelled:Arc<AtomicBool>,fence:Arc<RequestFence>,
    request:Option<Request>,bytes:Option<Vec<u8>>,cancel:Option<OwnedFd>,sent:bool,failed:bool,ready:bool,issued:bool,verification_attempted:bool,failure:Option<anyhow::Error>,
    packet:Option<RuntimePacket>,fd_stamps:Option<[(u64,u64,u64,u32,i64,i64);2]>,ack:Option<Ack>,baked:Option<Arc<BakedImageCas>>,inputs:Option<Arc<VerifiedBakedInputs>>,
    manifest_read:Option<BakedImageRead>,metadata_read:Option<VerifiedBakedRead>,metadata:BTreeMap<ObjectRef,Vec<u8>>,metadata_bytes:u64,tools:Vec<RuntimeToolData>}
impl NativeBackingState {
    fn staged(original:FinalizationBudget,cancelled:Arc<AtomicBool>)->Self{
        let fence=Arc::new(RequestFence{original:original.clone(),cancelled:cancelled.clone()});
        Self{original,cancelled,fence,request:None,bytes:None,cancel:None,sent:false,failed:false,ready:false,issued:false,verification_attempted:false,failure:None,
            packet:None,fd_stamps:None,ack:None,baked:None,inputs:None,manifest_read:None,metadata_read:None,metadata:BTreeMap::new(),metadata_bytes:0,tools:Vec::new()}
    }
    pub(super) fn signal_original_cancellation(&self)->Result<()>{
        if self.cancelled.load(Ordering::Acquire)||self.original.remaining().is_err(){self.original.cancel();if let Some(fd)=&self.cancel{signal_cancel(fd)?;}}Ok(())
    }
    fn read_metadata(&mut self,r:&ObjectRef)->Result<Vec<u8>>{
        self.fence.remaining()?;ensure!(matches!(r.kind,rudder_engine::protocol::ObjectKind::Directory|rudder_engine::protocol::ObjectKind::DirectoryShard|rudder_engine::protocol::ObjectKind::File)&&r.size_bytes<=65536,
            "runtime backing tool lookup metadata bound");
        if let Some(raw)=self.metadata.get(r){return Ok(raw.clone())}
        ensure!(self.metadata.len()<8192&&self.metadata_read.is_none(),"original runtime metadata read still pending/work bound");
        self.metadata_bytes=self.metadata_bytes.checked_add(r.size_bytes).context("runtime tool metadata work overflow")?;
        ensure!(self.metadata_bytes<=32*1024*1024,"runtime tool sparse metadata work bound");
        self.inputs.as_ref().unwrap().read_retaining(&mut self.metadata_read,r.clone(),self.fence.clone())?;
        let raw=self.metadata_read.as_ref().unwrap().verified_bytes()?.to_vec();self.fence.remaining()?;
        self.metadata.insert(r.clone(),raw.clone());self.metadata_read.take();Ok(raw)
    }
}
/// Retained before the first request/send. Actual Native client also keeps the
/// state on every error/drop; only the exact original ACK can issue a backing.
pub struct RuntimeBackingTransfer {client:Arc<RuntimeImageClient>,state:Arc<Mutex<NativeBackingState>>}
/// Non-Serde runtime-only role. Actual client plus original packet/FDs, actual
/// BakedImageCas, complete verified typed inputs and original reads stay held.
pub struct RuntimeEnvironmentBacking {client:Arc<RuntimeImageClient>,state:Arc<Mutex<NativeBackingState>>,
    baked:Arc<BakedImageCas>,inputs:Arc<VerifiedBakedInputs>,inventory:ObjectRef,manifest:ObjectRef,installer:ObjectRef,tools:Vec<RuntimeToolData>}

impl RuntimeImageClient {
    /// Genuine owner identity and currentness, not a bool/path/JSON role grant.
    pub fn assert_environment_scope(&self,config:&Arc<HostIngressConfigOwner>,registration:&Arc<RegisteredEngineOwner>)->Result<()> {
        ensure!(Arc::ptr_eq(config,&self.config)&&Arc::ptr_eq(registration,&self.registration),"runtime environment changed original config/registration owner");
        environment_allowed(self.config.data())?;self.current()
    }
    pub fn try_begin_environment_backing_retaining(self:&Arc<Self>,out:&mut Option<RuntimeBackingTransfer>,original:FinalizationBudget,cancelled:Arc<AtomicBool>)->Result<bool>{
        ensure!(out.is_none(),"original runtime backing output occupied");
        if cancelled.load(Ordering::Acquire){original.cancel();}original.remaining()?;
        environment_allowed(self.config.data())?;
        let mut client=match self.state.try_lock(){Ok(v)=>v,Err(TryLockError::WouldBlock)=>return Ok(false),Err(_)=>anyhow::bail!("original runtime client poisoned")};
        if client.active.is_some()||client.backing_active.is_some(){return Ok(false)}
        ensure!(client.backing_retained.is_none(),"runtime backing original already staged; retain that original owner");
        let state=Arc::new(Mutex::new(NativeBackingState::staged(original,cancelled)));
        self.uses.backing(&state)?;
        client.backing_active=Some(state.clone());client.backing_retained=Some(state.clone());
        *out=Some(RuntimeBackingTransfer{client:self.clone(),state:state.clone()});
        let mut state=state.try_lock().map_err(|_|anyhow::anyhow!("original runtime backing state unexpectedly borrowed/poisoned"))?;
        let result=(||{state.fence.remaining()?;self.current()?;
            let binding=self.registration.data();let inventory=self.config.data().image_preseed.as_ref().and_then(|p|p.runtime_mount.as_ref()).context("selected runtime backing policy absent")?.inventory.clone();
            state.cancel=Some(cancel_fd()?);state.request=Some(Request{version:1,kind:REQUEST_KIND.into(),nonce:uuid::Uuid::new_v4().to_string(),engine:binding.engine_instance_id.clone(),epoch:binding.binding.epoch.clone(),
                config_sha256:self.config.config_sha256().into(),registration_proof_sha256:binding.proof_sha256.clone(),inventory,constraint:state.original.constraint()});
            state.bytes=Some(canonical_json(state.request.as_ref().unwrap())?);state.fence.remaining()?;Ok(true)})();
        match result{Ok(v)=>Ok(v),Err(e)=>{state.failed=true;state.failure=Some(e);anyhow::bail!("original runtime backing capture failed; retain original")}}
    }
}
impl RuntimeBackingTransfer {
    pub fn poll_retaining(&mut self)->Result<bool>{
        let mut client=match self.client.state.try_lock(){Ok(v)=>v,Err(TryLockError::WouldBlock)=>return Ok(false),Err(_)=>anyhow::bail!("original runtime client poisoned")};
        let mut state=match self.state.try_lock(){Ok(v)=>v,Err(TryLockError::WouldBlock)=>return Ok(false),Err(_)=>anyhow::bail!("original runtime backing poisoned")};
        if state.ready{return Ok(true)}
        ensure!(!state.failed&&client.backing_active.as_ref().is_some_and(|s|Arc::ptr_eq(s,&self.state)),"runtime backing unknown/foreign lane; no replay");
        let result=(||->Result<bool>{state.fence.remaining()?;self.client.current()?;
            if !state.sent{match client.endpoint.send_data(state.bytes.as_ref().context("original backing request incomplete")?,&[state.cancel.as_ref().unwrap()]){
                Ok(false)=>return Ok(false),Ok(true)=>state.sent=true,Err(e)=>return Err(e)}}
            // Actual packet and both SCM_RIGHTS originals are stored BEFORE
            // parsing, credential, deadline, catalog or source constructors.
            if state.packet.is_none(){state.packet=client.endpoint.receive_data()?;}if state.packet.is_none(){return Ok(false)}
            ensure!(!state.verification_attempted,"original runtime backing verification interrupted; no replay");state.verification_attempted=true;
            verify_original_packet(&mut state,&self.client)?;
            state.fence.remaining()?;self.client.current()?;state.ready=true;Ok(true)})();
        match result{Ok(true)=>{client.backing_active=None;Ok(true)},Ok(false)=>Ok(false),Err(e)=>{
            state.failed=true;state.failure=Some(e);if let Some(cancel)=&state.cancel{let _=signal_cancel(cancel);}anyhow::bail!("original runtime backing failed; retain original packet/read owners")}}
    }
    pub fn take_backing_retaining(&mut self,out:&mut Option<Arc<RuntimeEnvironmentBacking>>)->Result<bool>{
        ensure!(out.is_none(),"runtime environment backing output occupied");if !self.poll_retaining()?{return Ok(false)}
        let mut state=self.state.try_lock().map_err(|_|anyhow::anyhow!("runtime backing publication busy/poisoned"))?;
        ensure!(state.ready&&!state.failed&&!state.issued,"runtime backing original ACK absent/already issued");state.fence.remaining()?;
        let ack=state.ack.as_ref().context("original runtime backing ACK absent")?;
        let owner=Arc::new(RuntimeEnvironmentBacking{client:self.client.clone(),state:self.state.clone(),baked:state.baked.as_ref().unwrap().clone(),inputs:state.inputs.as_ref().unwrap().clone(),
            inventory:ack.inventory.clone(),manifest:ack.manifest.clone(),installer:ack.installer_input.clone(),tools:state.tools.clone()});
        state.issued=true;*out=Some(owner);state.fence.remaining()?;self.client.current()?;Ok(true)
    }
}
impl RuntimeEnvironmentBacking {
    pub fn registration(&self)->&Arc<RegisteredEngineOwner>{&self.client.registration}
    pub fn config(&self)->&Arc<HostIngressConfigOwner>{&self.client.config}
    pub fn baked(&self)->&Arc<BakedImageCas>{&self.baked}
    pub fn inputs(&self)->&Arc<VerifiedBakedInputs>{&self.inputs}
    pub fn inventory(&self)->&ObjectRef{&self.inventory}
    pub fn manifest(&self)->&ObjectRef{&self.manifest}
    pub fn installer_input(&self)->&ObjectRef{&self.installer}
    pub fn runtime_tools_data(&self)->&[RuntimeToolData]{&self.tools}
    /// Projection DATA from this SAME actual Root client, never an execution
    /// grant. Actual original-barrier enrollment still supplies the capsule.
    pub fn projections_data(&self)->Result<Vec<rudder_engine::execution::ReadOnlyRuntime>>{self.assert_original_custody()?;self.client.projections_data()}
    pub fn matches_runtime_client(&self,client:&Arc<RuntimeImageClient>)->bool{Arc::ptr_eq(&self.client,client)}
    /// Physical original-owner check only, for retaining immutable old inputs
    /// after acquisition. No deadline/admission/retirement authority is minted.
    pub fn assert_original_custody(&self)->Result<()> {
        self.client.uses.contains_backing(&self.state)?;
        self.client.current()?;let state=match self.state.try_lock(){Ok(s)=>s,Err(TryLockError::WouldBlock)=>return Err(RuntimeBackingBusy.into()),Err(_)=>anyhow::bail!("original runtime backing custody poisoned")};
        ensure!(state.ready&&state.issued&&!state.failed,"original runtime backing ACK absent");packet_current(&state)?;
        self.inputs.binding_data()?;Ok(())
    }
    pub fn assert_current(&self,original:&dyn ImageImportFence)->Result<()> {
        original.remaining()?;self.client.uses.contains_backing(&self.state)?;self.client.current()?;
        loop{original.remaining()?;match self.state.try_lock(){Ok(s)=>{ensure!(s.ready&&s.issued&&!s.failed,"original runtime backing incomplete/fenced");packet_current(&s)?;break;},
            Err(TryLockError::WouldBlock)=>thread::sleep(original.remaining()?.min(Duration::from_millis(1))),Err(_)=>anyhow::bail!("original runtime backing custody poisoned")}}
        self.inputs.current(original)?;original.remaining()?;Ok(())
    }
}

fn verify_original_packet(state:&mut NativeBackingState,client:&RuntimeImageClient)->Result<()> {
    let packet=state.packet.as_ref().unwrap();let request=state.request.as_ref().unwrap();let binding=client.registration.data();
    check_reply_sender(packet,client.service.pid)?;
    let ack:Ack=serde_json::from_slice(&packet.bytes)?;let policy=client.config.data().image_preseed.as_ref().context("qualified runtime policy absent")?;
    ensure!(ack.version==1&&ack.kind==ACK_KIND&&ack.nonce==request.nonce&&ack.engine==request.engine&&ack.epoch==request.epoch
        &&ack.config_sha256==request.config_sha256&&ack.registration_proof_sha256==request.registration_proof_sha256&&ack.boot_id==binding.boot_id
        &&ack.inventory==request.inventory&&ack.manifest.digest==policy.full_manifest_sha256&&ack.mount_id>0&&ack.selected_executables.len()==4,
        "runtime backing ACK changed original scope/catalog/manifest");
    state.ack=Some(ack);state.fence.remaining()?;
    let packet=state.packet.as_ref().unwrap();let root=packet.fds[0].try_clone()?;let inventory=packet.fds[1].try_clone()?;
    state.fd_stamps=Some([stamp(&File::from(root.try_clone()?))?,stamp(&File::from(inventory.try_clone()?))?]);
    BakedImageCas::capture_retaining(&mut state.baked,root,inventory,request.inventory.clone(),state.fence.clone())?;
    let actual=state.baked.as_ref().unwrap().inventory_data()?;let ack=state.ack.as_ref().unwrap();
    ensure!(actual.roots.manifest==ack.manifest&&actual.roots.installer_input==ack.installer_input,"original runtime backing installer differs");
    VerifiedBakedInputs::capture_retaining(&mut state.inputs,state.baked.as_ref().unwrap().clone(),state.fence.clone())?;
    state.baked.as_ref().unwrap().read_manifest_retaining(&mut state.manifest_read,state.fence.clone())?;
    let manifest:crate::image_preseed_data::ImageManifestData=serde_json::from_slice(state.manifest_read.as_ref().unwrap().verified_bytes()?)?;
    ensure!(manifest.format_version==1&&manifest.installer_input==actual.roots.installer_input&&manifest.files.len()<=65536,"runtime full manifest differs");
    let paths=[("rudder-native-runtime","package/dist/native/rudder-native",ENGINE_PATH),
        ("rudder-jj-runtime","package/dist/native/rudder-jj","/opt/rudder/dist/native/rudder-jj"),
        ("rudder-storage-runtime","usr/local/bin/rudder-root-storage",SERVICE_PATH),
        ("rudder-launcher-runtime","usr/local/bin/rudder-machine-root",LAUNCHER_PATH)];
    for (i,(name,path,host)) in paths.into_iter().enumerate(){state.fence.remaining()?;
        let selected=&state.ack.as_ref().unwrap().selected_executables[i];let record=manifest.files.iter().find(|r|r.path==Path::new(host)).context("actual selected runtime absent from complete manifest")?;
        ensure!(selected.installer_path==path&&selected.sha256==record.sha256&&selected.size_bytes==record.size_bytes,"actual selected runtime differs from original Root whole-file check");
        let content=lookup_runtime_file(state,&actual.roots.installer_input,path)?;
        ensure!(content==record.content,"selected runtime FileNode differs from actual installer tree");
        state.tools.push(RuntimeToolData{name:name.into(),installer_path:path.into(),content,sha256:record.sha256.clone(),size_bytes:record.size_bytes});
    }
    state.fence.remaining()?;client.current()?;packet_current(state)?;state.manifest_read.take();Ok(())
}
fn packet_current(state:&NativeBackingState)->Result<()> {
    let packet=state.packet.as_ref().context("actual original backing packet absent")?;
    let expected=state.fd_stamps.as_ref().context("actual original backing FD witness absent")?;
    ensure!(packet.fds.len()==2,"actual original backing FD set changed");
    for (i,fd) in packet.fds.iter().enumerate(){ensure!(stamp(&File::from(fd.try_clone()?))?==expected[i],"actual original backing FD changed");}Ok(())
}
fn check_reply_sender(packet:&RuntimePacket,service_pid:u32)->Result<()> {
    ensure!(packet.credentials.pid==service_pid as i32&&packet.credentials.uid==0&&packet.credentials.gid==0&&packet.fds.len()==2,
        "runtime backing ACK is not original Root service/descriptors");Ok(())
}
struct SourceDirectory<'a>(RefCell<&'a mut NativeBackingState>);
impl DirectoryStore for SourceDirectory<'_>{fn read(&self,r:&ObjectRef)->Result<Vec<u8>>{self.0.borrow_mut().read_metadata(r)}
    fn put(&self,_:rudder_engine::protocol::ObjectKind,_:&[u8])->Result<ObjectRef>{anyhow::bail!("runtime input lookup is read-only")}}
fn lookup_runtime_file(state:&mut NativeBackingState,root:&ObjectRef,path:&str)->Result<ObjectRef>{
    let parts=path.split('/').collect::<Vec<_>>();ensure!(!parts.is_empty()&&parts.len()<=128,"runtime executable path depth");let mut root=root.clone();
    for (i,part) in parts.iter().enumerate(){let reader=SourceDirectory(RefCell::new(&mut *state));let entry=PagedDirectory::new(&reader).lookup(&root,part.as_bytes())?.context("actual runtime installer path absent")?;
        match entry.node{EntryNode::Directory{object}if i+1<parts.len()=>root=object,
            EntryNode::File{object,mode,..}if i+1==parts.len()=>{ensure!(mode&0o111!=0,"actual selected runtime is not executable");return Ok(object)},_=>anyhow::bail!("runtime tool path is not declared ordinary file")}}
    anyhow::bail!("runtime tool path empty")
}

#[cfg(test)]#[path="root_runtime_backing_tests.rs"]mod tests;
