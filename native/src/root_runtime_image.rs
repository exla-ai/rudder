//! Private actual machine bootstrap -> completed registration -> Root runtime
//! issuer. No saved receipt, uid0 actor, image path or binding DATA constructor.
//! Neither Drop nor a lost ACK unmounts, joins, closes a reader or frees capacity.
use super::{host_ingress::{HostIngressConfig,HostIngressConfigOwner,RootHostControl},engine_registration::{RootRegistration,RegisteredEngineOwner,CompletedRuntimeRegistration},*};
use rudder_engine::{execution::{runtime_image::{self as wire,RuntimeEndpoint,RuntimePacket,RuntimeImageCapsuleData,RuntimeExecutableData,RuntimeImageHandoffAttachment,COMPONENTS},handoff::ManagedExecutionStartBarrier},
    store::{LocalStore,ImageImportFence,baked_image::{BakedImageCas,BakedImageRead,BAKED_IMAGE_DIRECTORY}},
    vfs::{runtime_image::{ReadonlyRuntimeImage,RuntimeImageReadRestriction},linux::metadata::{RootMetadataNamespace,RetainedMetadataMount,MetadataMountMode}}};
use std::{collections::BTreeMap,os::unix::fs::DirBuilderExt,path::PathBuf,sync::{Mutex,TryLockError}};
use sha2::{Digest,Sha256};

const SERVICE_ROOT:&str="/run/rudder-root-storage";
const RUNTIME_ROOT:&str="/run/rudder-root-storage/runtime-image-v1";
const MAX_HELPERS:usize=256;
#[path="root_runtime_ready.rs"]
mod ready;
pub use ready::{RuntimeReadyObservation,RuntimeReadyObservationData};
#[path="root_runtime_qualification.rs"]
pub mod qualification;
#[path="root_compiler_sdk.rs"]
pub mod compiler_sdk;
#[path="compiler_sdk_client.rs"]
mod compiler_client;
pub use compiler_client::{CompilerSdkClient,CompilerSdkBuildOutcome,RetainedCompilerSdkBuild,CompilerSdkBuildFailure,CompilerSdkJobDisposal,CompilerSdkJobSettled};
#[path="compiler_sdk_job_protocol.rs"]
mod sdk_job_protocol;
#[path="root_compiler_qualification.rs"]
pub mod compiler_qualification;
#[path="root_runtime_backing.rs"]
pub mod environment;
#[path="root_runtime_uses.rs"]
mod uses;
pub use uses::{RuntimePreparationUse,RuntimeNativeUseData};
#[derive(Clone,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Enable {version:u32,kind:String,bootstrap_nonce:String,request_nonce:String,constraint:FinalizationConstraint}
#[derive(Clone,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct EnableAck {version:u32,kind:String,bootstrap_nonce:String,request_nonce:String,engine_instance_id:String,epoch:String,mount_id:u64,
    #[serde(default,skip_serializing_if="Option::is_none")]compiler_sdk:Option<compiler_sdk::CompilerSdkEnableData>,
    inventory:rudder_engine::protocol::ObjectRef,manifest:rudder_engine::protocol::ObjectRef,installer_input:rudder_engine::protocol::ObjectRef,selected_executables:Vec<RuntimeExecutableData>}
#[derive(Clone,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct HelperRequest {version:u32,request_nonce:String,binding_sha256:String,engine_instance_id:String,epoch:String,constraint:FinalizationConstraint}
#[derive(Clone,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct HelperAck {version:u32,request_nonce:String,binding_sha256:String,engine_instance_id:String,epoch:String,helper_pid:u32,helper_start_ticks:String}
fn digest(bytes:&[u8])->String{format!("{:x}",Sha256::digest(bytes))}
fn stamp(file:&File)->Result<(u64,u64,u64,u32,i64,i64)>{let m=file.metadata()?;Ok((m.dev(),m.ino(),m.len(),m.mode(),m.ctime(),m.ctime_nsec()))}
// Linux keeps /proc/PID (0555) owned by the effective UID even at
// dumpable=0. The protected fd directory (0500) follows task_dump_owner's
// nondumpable ownership rule; inspect that directory, never its entries.
fn protected_descriptor_directory_ready(pid: u32) -> Result<bool> {
    let metadata = fs::symlink_metadata(format!("/proc/{pid}/fd"))?;
    ensure!(metadata.is_dir() && metadata.mode() & 0o7777 == 0o500,
        "runtime original descriptor directory role/mode differs");
    match (metadata.uid(), metadata.gid()) {
        (0, 0) => Ok(true),
        (1500, 1500) => Ok(false),
        _ => anyhow::bail!("runtime original descriptor directory ownership differs"),
    }
}
fn require_protected_descriptor_directory(pid: u32) -> Result<()> {
    ensure!(protected_descriptor_directory_ready(pid)?,
        "runtime original engine lost protected descriptor floor");
    Ok(())
}
fn original_stat(pid:u32)->Result<(u32,String)>{let bytes=storage::proc_bytes(&format!("/proc/{pid}/stat"),4096)?;let text=std::str::from_utf8(&bytes)?;let(_,fields)=text.rsplit_once(')').context("runtime original stat incomplete")?;let fields:Vec<_>=fields.split_whitespace().collect();
    ensure!(fields.len()>=20,"runtime original stat fields absent");Ok((fields[1].parse()?,fields[19].into()))}
fn protected_directory(path:&Path)->Result<File>{ensure!(path.canonicalize()?==path,"runtime protected directory redirects");
    for ancestor in path.ancestors(){let m=fs::symlink_metadata(ancestor)?;ensure!(m.is_dir()&&!m.file_type().is_symlink()&&m.uid()==0&&m.gid()==0&&m.mode()&0o022==0,"runtime original directory is not image Root custody");}
    Ok(OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY|libc::O_NOFOLLOW|libc::O_CLOEXEC).open(path)?)}
// Fixed identifiers only. Neither a component pathname nor an underlying
// error's Display/Debug participates in the public bootstrap diagnostic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RuntimeComponent {
    Usr, Package, Model, Certificates, Alternatives, LoaderCache,
    Timezone, SslConfig, PythonSite,
}
fn runtime_component(name: &str) -> Result<RuntimeComponent> {
    Ok(match name {
        "usr" => RuntimeComponent::Usr,
        "package" => RuntimeComponent::Package,
        "model" => RuntimeComponent::Model,
        "certificates" => RuntimeComponent::Certificates,
        "alternatives" => RuntimeComponent::Alternatives,
        "loader-cache" => RuntimeComponent::LoaderCache,
        "timezone" => RuntimeComponent::Timezone,
        "ssl-config" => RuntimeComponent::SslConfig,
        "python-site" => RuntimeComponent::PythonSite,
        _ => return Err(anyhow::Error::new(BootstrapCheck::RuntimeComponentSet)),
    })
}
fn readonly_at(parent:&OwnedFd,name:&str,directory:bool)->Result<OwnedFd>{ensure!(!name.is_empty()&&!name.contains(['/', '\0'])&&name!="."&&name!="..","runtime child name invalid");let name=CString::new(name)?;
    let raw=unsafe{libc::openat(parent.as_raw_fd(),name.as_ptr(),libc::O_PATH|libc::O_NOFOLLOW|libc::O_CLOEXEC|if directory{libc::O_DIRECTORY}else{0})};
    ensure!(raw>=0,"runtime original child open failed");let fd=unsafe{OwnedFd::from_raw_fd(raw)};let m=File::from(fd.try_clone()?).metadata()?;ensure!(!m.file_type().is_symlink()&&(if directory{m.is_dir()}else{m.is_file()}),"runtime child role differs");Ok(fd)}
fn readonly_component_at(parent:&OwnedFd,name:&str,directory:bool,component:RuntimeComponent)->Result<OwnedFd>{
    let name=proof_check(BootstrapCheck::RuntimeComponentName(component),||{
        ensure!(!name.is_empty()&&!name.contains(['/', '\0'])&&name!="."&&name!="..","runtime child name invalid");
        Ok(CString::new(name)?)
    })?;
    let raw=unsafe{libc::openat(parent.as_raw_fd(),name.as_ptr(),libc::O_PATH|libc::O_NOFOLLOW|libc::O_CLOEXEC|if directory{libc::O_DIRECTORY}else{0})};
    if raw<0 {
        // Capture errno before any other syscall. Retain its actual kind/code;
        // the diagnostic projector never formats the private error chain.
        return Err(anyhow::Error::new(io::Error::last_os_error())
            .context(BootstrapCheck::RuntimeComponentOpen(component)))
    }
    let fd=unsafe{OwnedFd::from_raw_fd(raw)};
    let duplicate=proof_check(BootstrapCheck::RuntimeComponentDuplicate(component),||Ok(fd.try_clone()?))?;
    let m=proof_check(BootstrapCheck::RuntimeComponentMetadata(component),||Ok(File::from(duplicate).metadata()?))?;
    proof_check(BootstrapCheck::RuntimeComponentRole(component),||{
        ensure!(!m.file_type().is_symlink()&&(if directory{m.is_dir()}else{m.is_file()}),"runtime child role differs");Ok(())
    })?;
    Ok(fd)
}
fn check_runtime_component_mount(fd:&OwnedFd,mount_id:u64,component:RuntimeComponent)->Result<()>{
    let data=proof_check(BootstrapCheck::RuntimeComponentFdData(component),||wire::fd_data(fd))?;
    proof_check(BootstrapCheck::RuntimeComponentMount(component),||{
        ensure!(data.mount_id==mount_id,"runtime component escaped original FUSE mount");Ok(())
    })
}
fn read_control(file:&File,max:usize,original:&FinalizationBudget)->Result<Vec<u8>>{let m=file.metadata()?;ensure!(m.len()>0&&m.len()<=max as u64,"runtime control DATA exceeds bound");let before=stamp(file)?;let mut bytes=vec![0;m.len()as usize];let mut at=0;
    while at<bytes.len(){original.remaining()?;let end=(at+65536).min(bytes.len());let n=file.read_at(&mut bytes[at..end],at as u64)?;ensure!(n>0,"runtime control read incomplete");at+=n;}
    original.remaining()?;ensure!(stamp(file)?==before,"runtime original control file changed");Ok(bytes)}
fn cancel_fd()->Result<OwnedFd>{let fd=unsafe{libc::eventfd(0,libc::EFD_NONBLOCK|libc::EFD_CLOEXEC)};ensure!(fd>=0,"runtime original cancellation FD unavailable");Ok(unsafe{OwnedFd::from_raw_fd(fd)})}
fn check_cancel(fd:&OwnedFd)->Result<()>{ensure!(fs::read_link(format!("/proc/self/fd/{}",fd.as_raw_fd()))?==Path::new("anon_inode:[eventfd]"),"runtime original cancellation is not eventfd");
    let mut p=libc::pollfd{fd:fd.as_raw_fd(),events:libc::POLLIN,revents:0};ensure!(unsafe{libc::poll(&mut p,1,0)}==0,"runtime original helper request cancelled");Ok(())}
fn signal_cancel(fd:&OwnedFd)->Result<()>{let one=1u64;let n=unsafe{libc::write(fd.as_raw_fd(),(&one as *const u64).cast(),8)};
    ensure!(n==8||(n<0&&io::Error::last_os_error().kind()==io::ErrorKind::WouldBlock),"runtime original cancellation signal outcome unknown");Ok(())}
struct CaptureFence(FinalizationBudget);
impl ImageImportFence for CaptureFence{fn remaining(&self)->Result<Duration>{self.0.remaining()}}

/// Original channel/FD owner retained before service-spawn effects. An offer is
/// issued once, only after actual registered Native acceptance in Root's owner.
pub(crate) struct RootRuntimeParent {
    parent:RuntimeEndpoint,service:OwnedFd,engine:Option<OwnedFd>,service_engine:OwnedFd,
    original:Option<FinalizationBudget>,request:Option<Enable>,certificate:Option<OwnedFd>,config:Option<OwnedFd>,bytes:Option<Vec<u8>>,
    offered:bool,send_unknown:bool,failed:bool,pending:Option<RuntimePacket>,ready:bool,endpoint_offered:bool,
    accepted_ack:Option<EnableAck>,ready_evidence:Option<ready::ReadyEvidenceData>,ready_file:Option<OwnedFd>,ready_offered:bool,
}
impl RootRuntimeParent {
    pub(super) fn capture()->Result<Self>{let(a,b)=storage::bootstrap_socket_pair()?;let(c,d)=storage::bootstrap_socket_pair()?;
        Ok(Self{parent:RuntimeEndpoint::capture(a)?,service:b,engine:Some(c),service_engine:d,original:None,request:None,certificate:None,config:None,bytes:None,offered:false,send_unknown:false,failed:false,pending:None,ready:false,endpoint_offered:false,accepted_ack:None,ready_evidence:None,ready_file:None,ready_offered:false})}
    pub(super) fn inherited(&self)->[(&OwnedFd,i32);2]{[(&self.service,5),(&self.service_engine,6)]}
    pub(super) fn poll_retaining(&mut self,reg:&RootRegistration,host:&RootHostControl,service:&Process)->Result<bool>{
        ensure!(!self.failed&&!self.send_unknown,"runtime original Root offer outcome unknown/closed");
        if !reg.complete(){return Ok(false)}
        if self.request.is_none(){let original=FinalizationBudget::begin(Duration::from_secs(30))?;original.remaining()?;
            self.original=Some(original);let data=reg.completed_runtime_registration()?;
            ensure!(host.registration_config().image_preseed.as_ref().and_then(|p|p.runtime_mount.as_ref()).is_some(),"runtime image profile not explicitly configured");
            self.request=Some(Enable{version:1,kind:"runtime_image_enable_v1".into(),bootstrap_nonce:data.root_nonce.clone(),request_nonce:uuid::Uuid::new_v4().to_string(),constraint:self.original.as_ref().unwrap().constraint()});
            self.config=Some(host.duplicate_runtime_public_config()?);self.certificate=Some(super::sealed_bytes(&canonical_json(&data)?)?);
            self.bytes=Some(canonical_json(self.request.as_ref().unwrap())?);
        }
        if !self.offered {self.original.as_ref().unwrap().remaining()?;service.live()?;
            match self.parent.send_data(self.bytes.as_ref().unwrap(),&[self.config.as_ref().unwrap(),self.certificate.as_ref().unwrap()]){
                Ok(false)=>return Ok(false),Ok(true)=>self.offered=true,Err(e)=>{self.send_unknown=true;return Err(e)}}}
        if !self.ready {self.original.as_ref().unwrap().remaining()?;service.live()?;
            if self.pending.is_none(){self.pending=self.parent.receive_data()?;}let Some(p)=&self.pending else{return Ok(false)};
            ensure!(p.credentials.pid==service.pid as i32&&p.credentials.uid==0&&p.credentials.gid==0&&p.fds.is_empty(),"runtime actual service acknowledgment differs");
            let ack:EnableAck=serde_json::from_slice(&p.bytes)?;let q=self.request.as_ref().unwrap();let reg=reg.completed_runtime_registration()?;
            ensure!(ack.version==1&&ack.kind=="runtime_image_ready_v1"&&ack.bootstrap_nonce==q.bootstrap_nonce&&ack.request_nonce==q.request_nonce
                &&ack.engine_instance_id==reg.ack.engine_instance_id&&ack.epoch==reg.ack.binding.epoch&&ack.mount_id>0,"runtime original service ready binding differs");
            // Retain the ACTUAL parsed ACK and original RuntimePacket BEFORE
            // the final time/current checks; late/failed observations never issue.
            self.accepted_ack=Some(ack);let observed=super::boottime_stamp()?;
            self.ready_evidence=Some(ready::ReadyEvidenceData{version:1,kind:"original-runtime-enable-ready-evidence-v1".into(),
                request:q.clone(),ack:self.accepted_ack.as_ref().unwrap().clone(),service_pid:service.pid,service_start_ticks:original_stat(service.pid)?.1,
                registration_proof_sha256:reg.ack.proof_sha256.clone(),config_sha256:reg.ack.binding.config_sha256.clone(),boot_id:reg.ack.boot_id.clone(),root_ack_observed_boottime_nanoseconds:observed});
            self.original.as_ref().unwrap().remaining()?;service.live()?;self.ready=true;}
        Ok(self.ready)
    }
    pub(super) fn take_ready_evidence_retaining(&mut self)->Result<OwnedFd>{
        ensure!(self.ready&&!self.failed&&!self.ready_offered,"runtime original ready evidence absent/already offered");
        self.original.as_ref().unwrap().remaining()?;
        if self.ready_file.is_none(){let bytes=canonical_json(self.ready_evidence.as_ref().context("runtime actual ready ACK evidence absent")?)?;
            ensure!(bytes.len()<=ready::READY_LIMIT,"runtime ready evidence exceeds bound");self.ready_file=Some(super::sealed_bytes(&bytes)?);}
        self.original.as_ref().unwrap().remaining()?;let copy=self.ready_file.as_ref().unwrap().try_clone()?;self.ready_offered=true;Ok(copy)
    }
    pub(super) fn take_engine_endpoint_retaining(&mut self)->Result<OwnedFd>{ensure!(self.ready&&!self.endpoint_offered,"runtime original service ready ACK absent/already offered");
        self.original.as_ref().unwrap().remaining()?;self.endpoint_offered=true;Ok(self.engine.as_ref().context("runtime engine endpoint absent")?.try_clone()?)}
    pub(super) fn endpoint_was_offered(&self)->bool{self.endpoint_offered}
    pub(super) fn is_closed(&self)->bool{self.failed}
    pub(super) fn close_admission(&mut self){self.failed=true;}
    pub(super) fn original(&self)->Result<&FinalizationBudget>{self.original.as_ref().context("runtime original enable request absent")}
}

/// All inherited original descriptors, acquired before any runtime mount/view
/// effects. This private type can only be built by the fixed service entry.
struct RuntimeBootstrapProof {raw:Vec<OwnedFd>,root:Process,engine:Process,own:Process,root_image:Image,engine_image:Image,service_image:Image,data:BootstrapData,boot:Vec<u8>}
#[derive(Clone, Copy, Debug)]
enum BootstrapCheck {
    InheritedDescriptors,
    ParentEndpoint,
    EngineEndpoint,
    RootProcess,
    ServiceProcess,
    EngineProcess,
    LauncherImage,
    SealedData,
    Boot,
    EngineImage,
    ServiceImage,
    BackingProcess,
    BackingFloor,
    BackingImage,
    BackingNamespace,
    BackingBoot,
    ProcessLiveness,
    CredentialFloor,
    EngineProtectedProc,
    LauncherParentage,
    ProcessImages,
    RootIdMaps,
    ServiceIdMaps,
    EngineIdMaps,
    UserNamespace,
    MountNamespace,
    PidNamespace,
    RuntimeMountOriginal,
    RuntimeMountOfferDescriptors,
    RuntimeMountPublicConfig,
    RuntimeMountRegistration,
    RuntimeMountRegisteredImages,
    RuntimeMountPolicy,
    RuntimeMountAdmission,
    RuntimeMountBacking,
    RuntimeMountBakedInventory,
    RuntimeMountManifest,
    RuntimeMountStorage,
    RuntimeMountReadFloor,
    RuntimeMountFuse,
    RuntimeMountComponents,
    RuntimeComponentSet,
    RuntimeComponentName(RuntimeComponent),
    RuntimeComponentClock(RuntimeComponent),
    RuntimeComponentOpen(RuntimeComponent),
    RuntimeComponentDuplicate(RuntimeComponent),
    RuntimeComponentMetadata(RuntimeComponent),
    RuntimeComponentRole(RuntimeComponent),
    RuntimeComponentFdData(RuntimeComponent),
    RuntimeComponentMount(RuntimeComponent),
    RuntimeMountSelectedElves,
    RuntimeMountSelectedBinding,
    RuntimeMountReady,
    RuntimeMountSdk,
    SdkPrepareOriginal,
    SdkCatalog,
    SdkPlaceholder,
    SdkBackingDescriptors,
    SdkSourceBinding,
    SdkBakedInventory,
    SdkVerifiedManifest,
    SdkSemanticInputs,
    SdkToolchains,
    SdkArtifactBinding,
    SdkStorage,
    SdkReadFloor,
    SdkFuse,
    SdkMountData,
    SdkQualification,
    SdkReady,
    SdkHelperEntry,
    SdkHelperConnect,
    SdkHelperParent,
    SdkHelperParentImage,
    SdkHelperCapsule,
    SdkHelperServiceImage,
    SdkHelperLauncher,
    SdkHelperLauncherImage,
    SdkHelperFloor,
    SdkHelperWorkspace,
    SdkHelperNamespaceWorkload,
    SdkHelperResultSend,
    SdkHelperExitUnknown,
    SdkHelperExitedWithoutResult,
    SdkProcessOriginal,
    SdkProcessSpawn,
    SdkProcessCapture,
    SdkProcessPipes,
    SdkProcessChildExit,
    SdkProcessPeer,
    SdkProcessHelperFloor,
    SdkProcessHelperImage,
    SdkProcessHelperParent,
    SdkProcessCapsule,
    SdkProcessResult,
    SdkProcessWorkload,
    SdkProcessInterpreter,
    SdkProcessNamespaces,
    SdkProcessCapabilities,
    SdkProcessClock,
    SdkProcessExitEof,
}
impl std::fmt::Display for BootstrapCheck {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for BootstrapCheck {}
// A typed context is the only error-chain content the public diagnostic reads.
// Display/Debug for the underlying error (including private paths) is never used.
pub(crate) fn bootstrap_failure_check(error: &anyhow::Error) -> Option<String> {
    error
        .downcast_ref::<BootstrapCheck>()
        .map(|check| format!("{check:?}"))
}
fn proof_check<T>(check: BootstrapCheck, operation: impl FnOnce() -> Result<T>) -> Result<T> {
    operation().context(check)
}
// Add a fixed phase only when a more precise typed check is absent. No raw
// error text is inspected, and the original chain stays intact for its owner.
fn bootstrap_failure_context(error: anyhow::Error, fallback: BootstrapCheck) -> anyhow::Error {
    if error.downcast_ref::<BootstrapCheck>().is_some() { error } else { error.context(fallback) }
}
// The SDK carrier retains the original error. This separate diagnostic carries
// only a fixed check and an OS error reconstructed from errno/kind, never a
// private path/message or an authority/receipt derived from that error.
fn retained_bootstrap_diagnostic(error: &anyhow::Error, fallback: BootstrapCheck) -> anyhow::Error {
    let check = error.downcast_ref::<BootstrapCheck>().copied().unwrap_or(fallback);
    match error.chain().find_map(|cause| cause.downcast_ref::<io::Error>()) {
        Some(os) => {
            let fixed = match os.raw_os_error() {
                Some(errno) => io::Error::from_raw_os_error(errno),
                None => io::Error::from(os.kind()),
            };
            anyhow::Error::new(fixed).context(check)
        }
        None => anyhow::Error::new(check),
    }
}
impl RuntimeBootstrapProof {
    fn capture(raw: Vec<OwnedFd>, engine: &Process) -> Result<Self> {
        proof_check(BootstrapCheck::InheritedDescriptors, || {
            ensure!(raw.len() == 8, "runtime inherited bootstrap set incomplete");
            Ok(())
        })?;
        // raw: parent channel, engine channel, launcher pidfd/exe, sealed data,
        // actual USER/MNT/PID namespace FDs. No serialized FD reconstruction.
        let root = proof_check(BootstrapCheck::RootProcess, || {
            Process::capture(raw[2].try_clone()?)
        })?;
        let own = proof_check(BootstrapCheck::ServiceProcess, Process::current)?;
        let engine = proof_check(BootstrapCheck::EngineProcess, || {
            Process::capture(engine.fd().try_clone()?)
        })?;
        let root_image = proof_check(BootstrapCheck::LauncherImage, || {
            let image = Image::capture(Path::new(LAUNCHER_PATH))?;
            image.matches_fd(&raw[3])?;
            Ok(image)
        })?;
        let value = Self {
            data: proof_check(BootstrapCheck::SealedData, || read_data(&raw[4]))?,
            boot: proof_check(BootstrapCheck::Boot, || {
                storage::proc_bytes("/proc/sys/kernel/random/boot_id", 64)
            })?,
            raw,
            root,
            engine,
            own,
            root_image,
            engine_image: proof_check(BootstrapCheck::EngineImage, || {
                Image::capture(Path::new(ENGINE_PATH))
            })?,
            service_image: proof_check(BootstrapCheck::ServiceImage, || {
                Image::capture(Path::new(SERVICE_PATH))
            })?,
        };
        value.current()?;
        Ok(value)
    }
    fn backing_current(&self) -> Result<()> {
        proof_check(BootstrapCheck::BackingProcess, || {
            self.own.live()?;
            ensure!(
                self.own.pid == std::process::id()
                    && unsafe { libc::getuid() } == 0
                    && unsafe { libc::geteuid() } == 0,
                "runtime original service read owner changed"
            );
            Ok(())
        })?;
        proof_check(BootstrapCheck::BackingFloor, || {
            storage::floor(self.own.pid, 0)
        })?;
        proof_check(BootstrapCheck::BackingImage, || {
            self.service_image.matches_process(&self.own)
        })?;
        proof_check(BootstrapCheck::BackingNamespace, || {
            for (index, name, kind) in [
                (5, "user", libc::CLONE_NEWUSER),
                (6, "mnt", libc::CLONE_NEWNS),
                (7, "pid", libc::CLONE_NEWPID),
            ] {
                ensure!(
                    storage::id(&File::from(self.raw[index].try_clone()?))?
                        == storage::id(&storage::namespace(None, name, kind)?)?,
                    "runtime original backing namespace changed"
                );
            }
            Ok(())
        })?;
        proof_check(BootstrapCheck::BackingBoot, || {
            ensure!(
                self.boot == storage::proc_bytes("/proc/sys/kernel/random/boot_id", 64)?,
                "runtime original backing boot changed"
            );
            Ok(())
        })
    }
    fn current(&self) -> Result<()> {
        self.backing_current()?;
        proof_check(BootstrapCheck::ProcessLiveness, || {
            self.root.live()?;
            self.engine.live()?;
            self.own.live()?;
            ensure!(
                self.own.pid == std::process::id()
                    && unsafe { libc::geteuid() } == 0
                    && unsafe { libc::getuid() } == 0,
                "runtime issuer moved from original Root service"
            );
            Ok(())
        })?;
        proof_check(BootstrapCheck::CredentialFloor, || {
            storage::floor(self.root.pid, 0)?;
            storage::floor(self.own.pid, 0)?;
            storage::floor(self.engine.pid, 1500)
        })?;
        proof_check(BootstrapCheck::EngineProtectedProc, || {
            require_protected_descriptor_directory(self.engine.pid)
        })?;
        proof_check(BootstrapCheck::LauncherParentage, || {
            ensure!(
                original_stat(self.own.pid)?.0 == self.root.pid
                    && original_stat(self.engine.pid)?.0 == self.root.pid,
                "runtime original launcher parentage differs"
            );
            Ok(())
        })?;
        proof_check(BootstrapCheck::ProcessImages, || {
            self.root_image.matches_process(&self.root)?;
            self.root_image.matches_fd(&self.raw[3])?;
            self.engine_image.matches_process(&self.engine)?;
            self.service_image.matches_process(&self.own)
        })?;
        for (process, check) in [
            (&self.root, BootstrapCheck::RootIdMaps),
            (&self.own, BootstrapCheck::ServiceIdMaps),
            (&self.engine, BootstrapCheck::EngineIdMaps),
        ] {
            proof_check(check, || wire::require_initial_id_maps(process.pid))?;
        }
        for (index, name, kind, check) in [
            (
                5,
                "user",
                libc::CLONE_NEWUSER,
                BootstrapCheck::UserNamespace,
            ),
            (6, "mnt", libc::CLONE_NEWNS, BootstrapCheck::MountNamespace),
            (7, "pid", libc::CLONE_NEWPID, BootstrapCheck::PidNamespace),
        ] {
            proof_check(check, || {
                let held = File::from(self.raw[index].try_clone()?);
                ensure!(
                    unsafe { libc::ioctl(held.as_raw_fd(), libc::NS_GET_NSTYPE as libc::c_ulong) }
                        == kind,
                    "runtime original namespace FD type differs"
                );
                for pid in [self.root.pid, self.own.pid, self.engine.pid, 1] {
                    ensure!(
                        storage::id(&held)?
                            == storage::id(&storage::namespace(Some(pid), name, kind)?)?,
                        "runtime issuer left original global namespace"
                    );
                }
                Ok(())
            })?;
        }
        proof_check(BootstrapCheck::Boot, || {
            ensure!(
                self.boot == storage::proc_bytes("/proc/sys/kernel/random/boot_id", 64)?,
                "runtime original boot changed"
            );
            Ok(())
        })
    }
}

struct RuntimeReadAdmission {proof:Arc<RuntimeBootstrapProof>,config:File,config_stamp:(u64,u64,u64,u32,i64,i64),config_sha:String,registration:CompletedRuntimeRegistration,closed:AtomicBool,reads_closed:AtomicBool}
impl RuntimeReadAdmission {
    fn current(&self)->Result<()>{ensure!(!self.closed.load(Ordering::Acquire),"runtime image admission closed");self.proof.current()?;
        ensure!(stamp(&self.config)?==self.config_stamp,"runtime original public config FD changed");let named=fs::symlink_metadata(host_ingress::HOST_CONFIG_PATH)?;
        ensure!(!named.file_type().is_symlink()&&(named.dev(),named.ino())==(self.config_stamp.0,self.config_stamp.1),"runtime public config pathname replaced");
        let worker=self.registration.worker_tid;ensure!(worker>1&&worker!=self.proof.engine.pid&&original_stat(worker)?.1==self.registration.worker_start_ticks,"runtime original registered host worker exited/changed");
        let status=String::from_utf8(storage::proc_bytes(&format!("/proc/{worker}/status"),16384)?)?;
        ensure!(status.lines().any(|l|l.strip_prefix("Tgid:").is_some_and(|v|v.trim()==self.proof.engine.pid.to_string())),"runtime host worker belongs to another engine");storage::floor(worker,1500)?;
        for(index,name,kind)in [(5,"user",libc::CLONE_NEWUSER),(6,"mnt",libc::CLONE_NEWNS),(7,"pid",libc::CLONE_NEWPID)]{
            ensure!(storage::id(&File::from(self.proof.raw[index].try_clone()?))?==storage::id(&storage::namespace(Some(worker),name,kind)?)?,"runtime host worker left original global namespace");}
        Ok(())}
}
impl RuntimeImageReadRestriction for RuntimeReadAdmission {
    fn check_new_read(&self,original:&dyn ImageImportFence)->Result<()>{original.remaining()?;
        // This is an existing immutable read owner, not NEW task/shared
        // admission. A worker/registration fence cannot discard private work
        // or replace helper/native actor drain with a logical close flag.
        ensure!(!self.reads_closed.load(Ordering::Acquire),"runtime immutable read admission closed after genuine drain");self.proof.backing_current()?;
        original.remaining()?;Ok(())}
}
/// Original Root runtime mount/view/reader/process owners. Capture is private
/// to RootStorageService's real inherited bootstrap. No path or UID-only issuer.
pub(crate) struct RuntimeImageMountOwner {
    proof:Arc<RuntimeBootstrapProof>,original:FinalizationBudget,request:Enable,
    raw_offer:RuntimePacket,admission:Option<Arc<RuntimeReadAdmission>>,config:Option<HostIngressConfig>,
    baked:Option<Arc<BakedImageCas>>,original_backing:Option<environment::RootBackingDescriptors>,manifest_read:Option<BakedImageRead>,floor:Option<ReadonlyRuntimeImage>,
    local:Option<LocalStore>,mount:Option<RetainedMetadataMount>,root_fd:Option<OwnedFd>,components:Vec<OwnedFd>,
    compiler_sdk:Option<compiler_sdk::RootCompilerSdkOwner>,selected:Vec<RuntimeExecutableData>,pending_file:Option<File>,prepare_check:BootstrapCheck,ready:bool,failed:bool,
}
impl RuntimeImageMountOwner {
    fn capture_retaining(out:&mut Option<Self>,proof:Arc<RuntimeBootstrapProof>,packet:RuntimePacket,request:Enable)->Result<()>{ensure!(out.is_none(),"runtime mount original owner already exists");
        let original=FinalizationBudget::from_constraint(&request.constraint).map_err(|error|bootstrap_failure_context(error,BootstrapCheck::RuntimeMountOriginal))?;
        *out=Some(Self{proof,original,request,raw_offer:packet,admission:None,config:None,baked:None,original_backing:None,manifest_read:None,floor:None,local:None,mount:None,root_fd:None,components:Vec::new(),compiler_sdk:None,selected:Vec::new(),pending_file:None,prepare_check:BootstrapCheck::RuntimeMountOriginal,ready:false,failed:false});
        let owner=out.as_mut().unwrap();let result=owner.prepare().map_err(|error|bootstrap_failure_context(error,owner.prepare_check));if result.is_err(){owner.failed=true;if let Some(a)=&owner.admission{a.closed.store(true,Ordering::Release);a.reads_closed.store(true,Ordering::Release)}
            if let Some(floor)=&owner.floor{floor.close_admission()}if let Some(m)=&owner.mount{m.close_admission();}}result
    }
    fn prepare(&mut self)->Result<()>{self.prepare_check=BootstrapCheck::RuntimeMountOriginal;self.original.remaining()?;self.proof.current()?;
        self.prepare_check=BootstrapCheck::RuntimeMountOfferDescriptors;
        ensure!(self.raw_offer.fds.len()==2,"runtime Root offer must contain exact public config/certificate FDs");
        self.prepare_check=BootstrapCheck::RuntimeMountPublicConfig;
        let config=File::from(self.raw_offer.fds[0].try_clone()?);let m=config.metadata()?;
        ensure!(m.is_file()&&m.uid()==0&&m.gid()==0&&m.mode()&0o7777==0o600&&m.nlink()==1
            &&unsafe{libc::fcntl(config.as_raw_fd(),libc::F_GETFL)}&libc::O_ACCMODE==libc::O_RDONLY,"runtime public config is not original readonly Root-protected file");
        let bytes=read_control(&config,host_ingress::MAX_HOST_FRAME,&self.original)?;let data:HostIngressConfig=serde_json::from_slice(&bytes)?;data.validate_for_runtime_mount()?;
        self.prepare_check=BootstrapCheck::RuntimeMountRegistration;
        let certificate=super::read_sealed_bytes(&self.raw_offer.fds[1],16*1024)?;let reg:CompletedRuntimeRegistration=serde_json::from_slice(&certificate)?;
        ensure!(reg.version==1&&reg.kind=="completed_runtime_registration_v1"&&reg.root_nonce==self.proof.data.nonce&&reg.root_nonce==self.request.bootstrap_nonce
            &&reg.ack.root_nonce==reg.root_nonce&&reg.ack.config_bound(&data,&digest(&bytes))?&&reg.ack.boot_id==String::from_utf8(self.proof.boot.clone())?.trim(),"runtime completed original registration binding differs");
        self.prepare_check=BootstrapCheck::RuntimeMountRegisteredImages;
        ensure!(reg.ack.binding.native_sha256==self.proof.engine_image.sha256&&reg.ack.binding.launcher_sha256==self.proof.root_image.sha256
            &&reg.ack.binding.service_sha256==self.proof.service_image.sha256,"runtime original registered images differ");
        self.prepare_check=BootstrapCheck::RuntimeMountPolicy;
        let policy=data.image_preseed.as_ref().context("runtime qualified image policy absent")?;let runtime=policy.runtime_mount.as_ref().context("runtime image profile absent")?;
        self.prepare_check=BootstrapCheck::RuntimeMountAdmission;
        self.admission=Some(Arc::new(RuntimeReadAdmission{proof:Arc::clone(&self.proof),config_stamp:stamp(&config)?,config,config_sha:digest(&bytes),registration:reg,closed:AtomicBool::new(false),reads_closed:AtomicBool::new(false)}));
        self.config=Some(data.clone());self.admission.as_ref().unwrap().current()?;
        self.prepare_check=BootstrapCheck::RuntimeMountBacking;
        environment::RootBackingDescriptors::capture_retaining(&mut self.original_backing)?;
        let(root,inventory)=self.original_backing.as_ref().unwrap().duplicate_for_baked()?;
        let fence:Arc<dyn ImageImportFence>=Arc::new(CaptureFence(self.original.clone()));
        self.prepare_check=BootstrapCheck::RuntimeMountBakedInventory;
        BakedImageCas::capture_retaining(&mut self.baked,root,inventory,runtime.inventory.clone(),Arc::clone(&fence))?;
        let baked=self.baked.as_ref().unwrap();let inventory=baked.inventory_data()?;
        ensure!(inventory.roots.manifest.digest==policy.full_manifest_sha256,"runtime manifest differs from qualified image pin");
        self.prepare_check=BootstrapCheck::RuntimeMountManifest;
        baked.read_manifest_retaining(&mut self.manifest_read,Arc::clone(&fence))?;
        let manifest:crate::image_preseed_data::ImageManifestData=serde_json::from_slice(self.manifest_read.as_ref().unwrap().verified_bytes()?)?;
        ensure!(manifest.format_version==1&&manifest.installer_input==inventory.roots.installer_input&&manifest.files.len()<=65536,"runtime complete manifest/installer roots differ");
        self.prepare_check=BootstrapCheck::RuntimeMountStorage;
        self.original.remaining()?;protected_directory(Path::new(SERVICE_ROOT))?;fs::DirBuilder::new().mode(0o700).create(RUNTIME_ROOT)?;
        self.local=Some(LocalStore::open(Path::new(RUNTIME_ROOT).join("read-cache"))?);
        let restriction:Arc<dyn RuntimeImageReadRestriction>=self.admission.as_ref().unwrap().clone();
        self.prepare_check=BootstrapCheck::RuntimeMountReadFloor;
        ReadonlyRuntimeImage::capture_retaining(&mut self.floor,self.local.as_ref().unwrap().clone(),Arc::clone(baked),restriction,Arc::clone(&fence))?;
        ensure!(self.floor.as_ref().unwrap().installer_root()?==&inventory.roots.installer_input,"runtime floor changed exact installer root");
        self.prepare_check=BootstrapCheck::RuntimeMountFuse;
        let target=Path::new(RUNTIME_ROOT).join("floor");self.original.remaining()?;fs::DirBuilder::new().mode(0o700).create(&target)?;
        RetainedMetadataMount::capture_retaining(&mut self.mount,RootMetadataNamespace::capture()?,self.floor.as_ref().unwrap().view()?,target,
            MetadataMountMode::ReadOnlyRuntimeData,self.request.request_nonce.clone())?;
        self.mount.as_mut().unwrap().mount_retaining(&self.original,&AtomicBool::new(false))?;
        let(root,mount_id)=self.mount.as_ref().unwrap().duplicate_runtime_root_retaining()?;self.root_fd=Some(root);
        self.prepare_check=BootstrapCheck::RuntimeMountComponents;
        for(name,_,_,dir)in COMPONENTS{
            let component=runtime_component(name)?;
            proof_check(BootstrapCheck::RuntimeComponentClock(component),||{self.original.remaining()?;Ok(())})?;
            self.components.push(readonly_component_at(self.root_fd.as_ref().unwrap(),name,dir,component)?);
            check_runtime_component_mount(self.components.last().unwrap(),mount_id,component)?;
        }
        // Four selected installed native ELFs are fully read through the SAME
        // verified FUSE/chunk path before the floor's ready ACK. No host /usr
        // traversal/import/body copy or mtime-only executable acceptance.
        self.prepare_check=BootstrapCheck::RuntimeMountSelectedElves;
        for(installer_path,host_path)in [("package/dist/native/rudder-native",ENGINE_PATH),("package/dist/native/rudder-jj","/opt/rudder/dist/native/rudder-jj"),("usr/local/bin/rudder-root-storage",SERVICE_PATH),("usr/local/bin/rudder-machine-root",LAUNCHER_PATH)]{
            let record=manifest.files.iter().find(|f|f.path==Path::new(host_path)).context("selected native ELF absent from qualified full manifest")?;
            self.selected.push(RuntimeExecutableData{installer_path:installer_path.into(),sha256:record.sha256.clone(),size_bytes:record.size_bytes});
            let path=self.mount.as_ref().unwrap().root()?.join(installer_path);self.pending_file=Some(OpenOptions::new().read(true).custom_flags(libc::O_CLOEXEC|libc::O_NOFOLLOW).open(path)?);
            let file=self.pending_file.as_ref().unwrap();ensure!(file.metadata()?.len()==record.size_bytes,"runtime selected ELF size differs");let before=stamp(file)?;let mut hash=Sha256::new();let mut chunk=[0;65536];let mut at=0;
            while at<record.size_bytes{self.original.remaining()?;let n=file.read_at(&mut chunk[..usize::try_from((record.size_bytes-at).min(65536))?],at)?;ensure!(n>0,"runtime selected ELF read incomplete");hash.update(&chunk[..n]);at+=n as u64;}
            ensure!(stamp(file)?==before&&format!("{:x}",hash.finalize())==record.sha256,"runtime selected ELF whole SHA differs");self.pending_file.take();}
        self.prepare_check=BootstrapCheck::RuntimeMountSelectedBinding;
        let a=self.admission.as_ref().unwrap();ensure!(self.selected[0].sha256==a.registration.ack.binding.native_sha256&&self.selected[2].sha256==a.registration.ack.binding.service_sha256&&self.selected[3].sha256==a.registration.ack.binding.launcher_sha256,"runtime full-image selected ELF differs from original registration");
        self.prepare_check=BootstrapCheck::RuntimeMountReady;
        a.current()?;self.original.remaining()?;self.ready=true;
        self.prepare_check=BootstrapCheck::RuntimeMountSdk;
        if let Some(catalog)=&data.compiler_sdk {
            compiler_sdk::RootCompilerSdkOwner::capture_retaining(&mut self.compiler_sdk,self.proof.clone(),self.admission.as_ref().unwrap().clone(),self.original.clone(),catalog.clone(),data.compiler_qualification.is_some())?;
        }Ok(())
    }
    fn current(&self)->Result<()>{ensure!(self.ready&&!self.failed,"runtime original floor not ready/retained failure");self.admission.as_ref().context("runtime actual admission absent")?.current()?;self.mount.as_ref().unwrap().check_current()?;if let Some(sdk)=&self.compiler_sdk{sdk.current()?;}Ok(())}
    fn capsule_retaining(&mut self,session:&mut ServiceSession)->Result<bool>{self.current()?;session.original.remaining()?;let a=self.admission.as_ref().unwrap();let ack=&a.registration.ack;
        session.helper.as_ref().context("runtime original helper pidfd absent")?.live()?;let helper=session.helper.as_ref().unwrap();
        storage::floor(helper.pid,1500)?;ensure!(original_stat(helper.pid)?.0==self.proof.engine.pid,"runtime helper is not child of original engine");
        self.proof.engine_image.matches_process(helper)?;
        // The fixed receiver sets ND0 before connecting. Its protected fd
        // directory, rather than the always-effective-owned 0555 /proc/PID,
        // observes that floor. Before ND0, keep this same session pending.
        if !protected_descriptor_directory_ready(helper.pid)?{return Ok(false)}
        for(name,kind)in [("mnt",libc::CLONE_NEWNS),("user",libc::CLONE_NEWUSER),("pid",libc::CLONE_NEWPID)]{ensure!(storage::id(&storage::namespace(Some(helper.pid),name,kind)?)?==storage::id(&storage::namespace(Some(self.proof.engine.pid),name,kind)?)?,"runtime pending helper already entered guest namespace");}
        let inventory=self.baked.as_ref().unwrap().inventory_data()?;
        let data=RuntimeImageCapsuleData{compiler_sdk:self.compiler_sdk.as_ref().map(|s|s.enable_data().map(|d|d.mount)).transpose()?,version:1,request_nonce:session.request.request_nonce.clone(),binding_sha256:session.request.binding_sha256.clone(),engine_instance_id:ack.engine_instance_id.clone(),epoch:ack.binding.epoch.clone(),config_sha256:a.config_sha.clone(),registration_proof_sha256:ack.proof_sha256.clone(),boot_id:ack.boot_id.clone(),
            launcher_pid:self.proof.root.pid,service_pid:self.proof.own.pid,engine_pid:self.proof.engine.pid,helper_pid:helper.pid,helper_start_ticks:session.start.clone(),
            launcher_sha256:self.proof.root_image.sha256.clone(),service_sha256:self.proof.service_image.sha256.clone(),engine_sha256:self.proof.engine_image.sha256.clone(),
            inventory:self.config.as_ref().unwrap().image_preseed.as_ref().unwrap().runtime_mount.as_ref().unwrap().inventory.clone(),manifest:inventory.roots.manifest,installer_input:inventory.roots.installer_input,
            constraint:session.request.constraint.clone(),floor:wire::fd_data(self.root_fd.as_ref().unwrap())?,components:self.components.iter().map(wire::fd_data).collect::<Result<_>>()?,selected_executables:self.selected.clone()};
        data.validate_data()?;session.bytes=Some(canonical_json(&data)?);session.certificate=Some(super::sealed_bytes(session.bytes.as_ref().unwrap())?);
        session.fds.push(session.certificate.as_ref().unwrap().try_clone()?);session.fds.push(self.proof.root.fd().try_clone()?);session.fds.push(self.proof.raw[3].try_clone()?);
        session.fds.push(self.proof.own.fd().try_clone()?);session.fds.push(File::open(format!("/proc/{}/exe",self.proof.own.pid))?.into());session.fds.push(self.proof.engine.fd().try_clone()?);session.fds.push(File::open(format!("/proc/{}/exe",self.proof.engine.pid))?.into());session.fds.push(helper.fd().try_clone()?);
        for index in [5,6,7]{session.fds.push(self.proof.raw[index].try_clone()?);}session.fds.push(a.config.try_clone()?.into());session.fds.push(self.root_fd.as_ref().unwrap().try_clone()?);session.fds.push(session.packet.fds[2].try_clone()?);
        for component in &self.components{session.fds.push(component.try_clone()?);}if let Some(sdk)=&self.compiler_sdk{session.fds.push(sdk.duplicate_floor()?);}session.original.remaining()?;self.current()?;Ok(true)
    }
    fn close_admission(&self){if let Some(sdk)=&self.compiler_sdk{sdk.close_admission()}if let Some(a)=&self.admission{a.closed.store(true,Ordering::Release)}if let Some(m)=&self.mount{m.close_admission()}
        // Keep existing immutable reads and every floor/FD/worker owner. No
        // callable runtime actor-drain issuer exists yet, so reads_closed,
        // floor.close_admission and normal umount are NOT reachable here.
    }
}
struct ServiceSession {packet:RuntimePacket,request:HelperRequest,original:FinalizationBudget,helper:Option<Process>,start:String,endpoint:Option<RuntimeEndpoint>,certificate:Option<OwnedFd>,bytes:Option<Vec<u8>>,fds:Vec<OwnedFd>,sent:bool,unknown:bool,ack:Option<Vec<u8>>,ack_sent:bool,exit_observed:bool}
/// One ownership snapshot of the actual entry descriptor table. This MUST be
/// taken before constructors, filesystem/proc reads or any FD allocation. It
/// records absence permanently; later internal descriptors in5..12 can never
/// become inherited bootstrap inputs. No path, JSON or numeric reopen API.
pub(crate) struct CapturedRuntimeDescriptors {
    raw:[Option<OwnedFd>;8],mask:u8,fault:bool,transferred:bool,
}
impl CapturedRuntimeDescriptors {
    /// No FD-allocating call, filesystem read or heap collection precedes the
    /// complete table snapshot. fcntl only inspects/marks the EXISTING originals.
    /// Original handles are adopted before flag-setting failures are recorded.
    /// # Safety
    /// On the first call, slots5..12 must still be unclaimed inherited inputs:
    /// no Rust owner and no prior descriptor-allocating operation may exist for
    /// them. Repeated calls return a fault without touching the descriptor table.
    pub(crate) unsafe fn take_at_service_entry()->Self {
        static TAKEN:AtomicBool=AtomicBool::new(false);
        let mut captured=Self{raw:std::array::from_fn(|_|None),mask:0,fault:false,transferred:false};
        if TAKEN.swap(true,Ordering::AcqRel){captured.fault=true;return captured}
        for index in 0..8 {let raw=5+index as i32;let flags=unsafe{libc::fcntl(raw,libc::F_GETFD)};
            if flags<0 {if io::Error::last_os_error().raw_os_error()!=Some(libc::EBADF){captured.fault=true;}continue}
            captured.raw[index]=Some(unsafe{OwnedFd::from_raw_fd(raw)});captured.mask|=1u8<<index;
            if unsafe{libc::fcntl(raw,libc::F_SETFD,flags|libc::FD_CLOEXEC)}!=0{captured.fault=true;}
        }captured
    }
    pub(crate) fn is_absent_at_entry(&self)->bool{!self.fault&&!self.transferred&&self.mask==0}
    pub(crate) fn validate_shape(&self)->Result<()> {
        ensure!(!self.fault&&!self.transferred&&(self.mask==0||self.mask==u8::MAX),"runtime original inherited descriptor set partial/already captured/unavailable");Ok(())
    }
    fn take_complete_into_retaining(&mut self,out:&mut Vec<OwnedFd>)->Result<bool>{
        ensure!(out.is_empty(),"runtime original inherited destination occupied");self.validate_shape()?;
        if self.mask==0{return Ok(false)}
        // Caller stages the actual destination carrier BEFORE this transfer.
        // No allocation/open/reclassification can change the entry mask.
        for slot in &mut self.raw{out.push(slot.take().context("runtime original inherited owner incomplete")?);}
        self.transferred=true;Ok(true)
    }
}

/// Private service extension. Missing inherited set leaves legacy service
/// behavior unchanged. Partial/bad set is absorbing and retains every owner.
pub(crate) struct RootRuntimeService {raw:Vec<OwnedFd>,proof:Option<Arc<RuntimeBootstrapProof>>,parent:Option<RuntimeEndpoint>,engine:Option<RuntimeEndpoint>,offer:Option<RuntimePacket>,owner:Option<RuntimeImageMountOwner>,ack:Option<Vec<u8>>,ack_sent:bool,sessions:BTreeMap<String,ServiceSession>,compiler_jobs:Vec<compiler_sdk::jobs::RootBuildTransfer>,sdk_job_high_watermark:u64,backing_transfer:Option<environment::RootBackingTransfer>,pending:Option<RuntimePacket>,failed:bool}
impl RootRuntimeService {
    pub(crate) fn capture_entry_snapshot_retaining(out:&mut Option<Self>,captured:&mut CapturedRuntimeDescriptors,engine:&Process)->Result<()> {
        ensure!(out.is_none(),"runtime inherited slot occupied");captured.validate_shape()?;
        if captured.is_absent_at_entry(){return Ok(())}
        *out=Some(Self{raw:Vec::with_capacity(8),proof:None,parent:None,engine:None,offer:None,owner:None,ack:None,ack_sent:false,sessions:BTreeMap::new(),compiler_jobs:Vec::new(),sdk_job_high_watermark:0,backing_transfer:None,pending:None,failed:false});
        let s=out.as_mut().unwrap();ensure!(captured.take_complete_into_retaining(&mut s.raw)?,"runtime complete entry snapshot absent");
        // Validate ONLY the owned entry snapshot against the actual constructed
        // service/engine. Never inspect numeric slots after allocation.
        s.parent=Some(proof_check(BootstrapCheck::ParentEndpoint,||RuntimeEndpoint::capture(s.raw[0].try_clone()?))?);
        s.engine=Some(proof_check(BootstrapCheck::EngineEndpoint,||RuntimeEndpoint::capture(s.raw[1].try_clone()?))?);
        let raw=proof_check(BootstrapCheck::InheritedDescriptors,||Ok(s.raw.iter().map(|f|f.try_clone()).collect::<io::Result<_>>()?))?;
        s.proof=Some(Arc::new(RuntimeBootstrapProof::capture(raw,engine)?));Ok(())
    }
    pub(crate) fn close_admission(&mut self){self.failed=true;if let Some(owner)=&self.owner{owner.close_admission()}}
    pub(crate) fn poll_retaining(&mut self)->Result<bool>{ensure!(!self.failed,"runtime original service admission quarantined");let result=self.poll_inner();if result.is_err(){self.close_admission()}result}
    fn poll_inner(&mut self)->Result<bool>{let proof=self.proof.as_ref().context("runtime actual bootstrap chain incomplete")?;proof.current()?;
        if self.owner.is_none(){if self.offer.is_none(){self.offer=self.parent.as_ref().unwrap().receive_data()?;}let Some(packet)=self.offer.as_ref()else{return Ok(false)};
            ensure!(packet.credentials.pid==proof.root.pid as i32&&packet.credentials.uid==0&&packet.credentials.gid==0,"runtime enable is not actual original image launcher");
            let request:Enable=serde_json::from_slice(&packet.bytes)?;ensure!(request.version==1&&request.kind=="runtime_image_enable_v1"&&request.bootstrap_nonce==proof.data.nonce,"runtime enable original bootstrap binding differs");
            RuntimeImageMountOwner::capture_retaining(&mut self.owner,Arc::clone(proof),self.offer.take().unwrap(),request)?;
            let owner=self.owner.as_ref().unwrap();owner.current()?;let a=owner.admission.as_ref().unwrap();let baked=owner.baked.as_ref().unwrap();let inventory=baked.inventory_data()?;
            self.ack=Some(canonical_json(&EnableAck{version:1,kind:"runtime_image_ready_v1".into(),bootstrap_nonce:owner.request.bootstrap_nonce.clone(),request_nonce:owner.request.request_nonce.clone(),engine_instance_id:a.registration.ack.engine_instance_id.clone(),epoch:a.registration.ack.binding.epoch.clone(),mount_id:wire::fd_data(owner.root_fd.as_ref().unwrap())?.mount_id,
                inventory:baked.inventory_reference_data().clone(),manifest:inventory.roots.manifest,installer_input:inventory.roots.installer_input,selected_executables:owner.selected.clone(),compiler_sdk:owner.compiler_sdk.as_ref().map(|s|s.enable_data()).transpose()?})?);}
        let owner=self.owner.as_mut().unwrap();owner.current()?;
        if !self.ack_sent{owner.original.remaining()?;if !self.parent.as_ref().unwrap().send_data(self.ack.as_ref().unwrap(),&[])?{return Ok(false)}owner.original.remaining()?;self.ack_sent=true;return Ok(true)}
        let mut job_progress=false;
        for job in &mut self.compiler_jobs{job_progress|=job.poll_retaining(owner,self.engine.as_ref().unwrap())?;}
        // Removal requires actual known resource closure, not a sent build ACK.
        self.compiler_jobs.retain(|job|!job.resources_disposed());if job_progress{return Ok(true)}
        if let Some(backing)=&mut self.backing_transfer{match backing.poll_retaining(owner,self.engine.as_ref().unwrap()){
            Ok(true)=>return Ok(true),Ok(false)=>{},Err(_)=>{} // Original failed packet/FDs remain; existing helper/private reads continue.
        }}
        for session in self.sessions.values_mut(){if session.unknown||session.ack_sent{continue}
            // Per-helper expiry/cancellation/loss closes only this original
            // request. It never unmounts/retires the machine floor or treats a
            // process exit as the missing native actor/TLS drain proof.
            let result=(||->Result<bool>{session.original.remaining()?;check_cancel(&session.packet.fds[2])?;
                if session.helper.as_ref().context("runtime helper ownership incomplete")?.exited()?{session.exit_observed=true;anyhow::bail!("runtime original helper exited before handoff");}
                if session.bytes.is_none()&&!owner.capsule_retaining(session)?{return Ok(false)}
                if !session.sent{let fds:Vec<_>=session.fds.iter().collect();if !session.endpoint.as_ref().unwrap().send_data(session.bytes.as_ref().unwrap(),&fds)?{return Ok(false)}session.sent=true;
                    // These are transfer DUPLICATES only. Actual originals
                    // remain in floor/proof, session.helper/certificate/packet.
                    // Unknown send keeps every duplicate instead.
                    session.fds.clear();}
                session.original.remaining()?;check_cancel(&session.packet.fds[2])?;
                if !self.engine.as_ref().unwrap().send_data(session.ack.as_ref().unwrap(),&[])?{return Ok(false)}
                session.ack_sent=true;session.original.remaining()?;Ok(true)})();
            match result{Ok(true)=>return Ok(true),Ok(false)=>{},Err(_)=>{session.unknown=true;}}
        }
        if self.pending.is_none(){match self.engine.as_ref().unwrap().receive_disposal_retaining(&mut self.pending)?{
            wire::RuntimeDisposalRead::Pending=>return Ok(false),wire::RuntimeDisposalRead::Packet=>{},wire::RuntimeDisposalRead::ReadClosed{..}=>anyhow::bail!("original runtime engine endpoint closed; pending owners retained")}}
        let Some(packet)=self.pending.as_ref()else{return Ok(false)};
        ensure!(packet.credentials.pid==proof.engine.pid as i32&&packet.credentials.uid==1500&&packet.credentials.gid==1500,"runtime enrollment is not actual original engine");
        if compiler_sdk::jobs::is_request(&packet.bytes)?{if self.compiler_jobs.len()>=MAX_HELPERS{return Ok(false)}
            let staged=compiler_sdk::jobs::RootBuildTransfer::staged(self.pending.take().unwrap());self.compiler_jobs.push(staged);
            self.compiler_jobs.last_mut().unwrap().admit_original_serial_retaining(owner,&mut self.sdk_job_high_watermark);return Ok(true)}
        if environment::is_request(&packet.bytes)?{
            ensure!(self.backing_transfer.is_none(),"original runtime backing request already retained; no replay");
            self.backing_transfer=Some(environment::RootBackingTransfer::stage(self.pending.take().unwrap()));return Ok(true)
        }
        ensure!(packet.fds.len()==3,"runtime helper enrollment descriptor set differs");
        let request:HelperRequest=serde_json::from_slice(&packet.bytes)?;let a=owner.admission.as_ref().unwrap();
        ensure!(request.version==1&&request.engine_instance_id==a.registration.ack.engine_instance_id&&request.epoch==a.registration.ack.binding.epoch&&self.sessions.len()<MAX_HELPERS&&!self.sessions.contains_key(&request.request_nonce),"runtime enrollment scope/capacity differs; no replay");
        let original=FinalizationBudget::from_constraint(&request.constraint)?;let key=request.request_nonce.clone();let packet=self.pending.take().unwrap();
        self.sessions.insert(key.clone(),ServiceSession{packet,request,original,helper:None,start:String::new(),endpoint:None,certificate:None,bytes:None,fds:Vec::new(),sent:false,unknown:false,ack:None,ack_sent:false,exit_observed:false});
        let session=self.sessions.get_mut(&key).unwrap();session.helper=Some(Process::capture(session.packet.fds[0].try_clone()?)?);session.start=original_stat(session.helper.as_ref().unwrap().pid)?.1;
        session.endpoint=Some(RuntimeEndpoint::capture(session.packet.fds[1].try_clone()?)?);
        session.ack=Some(canonical_json(&HelperAck{version:1,request_nonce:session.request.request_nonce.clone(),binding_sha256:session.request.binding_sha256.clone(),engine_instance_id:a.registration.ack.engine_instance_id.clone(),epoch:a.registration.ack.binding.epoch.clone(),helper_pid:session.helper.as_ref().unwrap().pid,helper_start_ticks:session.start.clone()})?);Ok(true)
    }
}

struct ClientState {endpoint:RuntimeEndpoint,active:Option<Arc<Mutex<ClientRequest>>>,retained:Vec<Arc<Mutex<ClientRequest>>>,
    backing_active:Option<Arc<Mutex<environment::NativeBackingState>>>,backing_retained:Option<Arc<Mutex<environment::NativeBackingState>>>}
struct ClientRequest {original:FinalizationBudget,cancelled:Arc<AtomicBool>,helper:Arc<rudder_engine::execution::handoff::helper_process::HelperProcessWitness>,binding:rudder_engine::execution::ManagedExecutionBinding,
    request:Option<HelperRequest>,bytes:Option<Vec<u8>>,pidfd:Option<OwnedFd>,delivery:Option<OwnedFd>,receive:Option<OwnedFd>,cancel:Option<OwnedFd>,pid:u32,start:String,sent:bool,unknown:bool,pending:Option<RuntimePacket>,ready:bool,attached:bool}
/// Actual registered Native owner + original authenticated service endpoint.
/// No public path/JSON constructor. A pending/unknown request remains owned by
/// this machine client even if its task caller drops its output alias.
pub struct RuntimeImageClient {uses:uses::RuntimeUseLedger,compiler_sdk:Mutex<Option<Arc<CompilerSdkClient>>>,ready:ready::ReadyOrigin,state:Mutex<ClientState>,registration:Arc<RegisteredEngineOwner>,config:Arc<HostIngressConfigOwner>,service:Process,service_image:Image,service_exe:OwnedFd}
/// Per-helper actual request/carrier. No source/task/lease authority. Its only
/// transferable value is a transport FD which the original helper independently
/// authenticates against the actual Root service and sealed managed binding.
pub struct RuntimeHelperGrant {client:Arc<RuntimeImageClient>,state:Arc<Mutex<ClientRequest>>}
/// Exact original grant identity only, never proof that its task/readers joined.
/// No constructor, Clone, Serde or operation that polls/attaches a helper.
pub struct RuntimeHelperUseIdentity {client:Arc<RuntimeImageClient>,state:Arc<Mutex<ClientRequest>>}
/// One live appendable ledger owner; no per-poll retained history or final set.
#[must_use="retain the original runtime client and its appendable use ledger"]
pub struct RuntimeNativeUseObservation {client:Arc<RuntimeImageClient>,uses:uses::RuntimeUseObservation}
impl RuntimeNativeUseObservation {
    /// OFF App, fixed diagnostic DATA only. Arbitrary counts/IDs cannot provide
    /// identities here. These exact identities themselves grant no completion.
    pub fn sample_original_helpers(&self,actual:&[RuntimeHelperUseIdentity])->Result<Option<RuntimeNativeUseData>> {
        ensure!(actual.len()<=MAX_HELPERS,"original helper observation bound exceeded");
        let Some(sample)=self.uses.try_sample()? else {return Ok(None);};
        let mut data=sample.data()?;
        for (index,identity) in actual.iter().enumerate() {
            ensure!(Arc::ptr_eq(&identity.client,&self.client),"original runtime-use observation has foreign client");
            ensure!(!actual[..index].iter().any(|old|Arc::ptr_eq(&old.state,&identity.state)),"original helper observed twice");
            ensure!(sample.helpers.iter().any(|held|Arc::ptr_eq(held,&identity.state)),"observed helper absent from original ledger sample");
            data.associated_original_helpers+=1;
        }
        data.unmatched_helpers=data.helper_uses-data.associated_original_helpers;Ok(Some(data))
    }
}
impl RuntimeImageClient {
    pub(super) fn capture_original_retaining(out:&mut Option<Arc<Self>>,observation:&mut Option<RuntimeReadyObservation>,endpoint:OwnedFd,ready_fd:&OwnedFd,bootstrap_nonce:&str,registration:Arc<RegisteredEngineOwner>,config:Arc<HostIngressConfigOwner>,service:&OwnedFd,service_exe:&OwnedFd)->Result<()> {
        ensure!(out.is_none()&&observation.is_none(),"runtime actual client/observation already captured");registration.assert_current()?;config.assert_current()?;
        ensure!(config.data().image_preseed.as_ref().and_then(|p|p.runtime_mount.as_ref()).is_some(),"runtime image profile absent");
        let service=Process::capture(service.try_clone()?)?;let image=Image::capture(Path::new(SERVICE_PATH))?;image.matches_fd(service_exe)?;service.live()?;
        let ready=ready::ReadyOrigin::capture_from_original_bootstrap(ready_fd,&registration,&service,bootstrap_nonce)?;
        *out=Some(Arc::new(Self{uses:uses::RuntimeUseLedger::new(),compiler_sdk:Mutex::new(None),ready,state:Mutex::new(ClientState{endpoint:RuntimeEndpoint::capture(endpoint)?,active:None,retained:Vec::new(),backing_active:None,backing_retained:None}),registration,config,service,service_image:image,service_exe:service_exe.try_clone()?}));
        let client=out.as_ref().unwrap();client.current()?;client.ready.original_receipt_current()?;
        if client.config.data().compiler_sdk.is_some(){let mut slot=client.compiler_sdk.lock().unwrap_or_else(|p|p.into_inner());CompilerSdkClient::capture_retaining(&mut slot,client.clone())?;}
        *observation=Some(RuntimeReadyObservation::issue_on_original_client(client)?);Ok(())
    }
    /// Local absorbing admission only. Existing request polling, immutable
    /// DATA reads and SDK disposal keep their original custody. No Root packet,
    /// floor close, release, cancellation or capacity recycling is issued here.
    pub fn close_original_use_admission(&self)->Result<()> {self.uses.close()}
    /// Pure retained alias capture only. Do not close admission: previously
    /// admitted tasks may still be preparing their first helper enrollment.
    /// No currentness/clock/Root IO and no authority reconstructed from DATA.
    pub fn capture_native_use_observation_retaining(self:&Arc<Self>,out:&mut Option<RuntimeNativeUseObservation>)->Result<()> {
        if let Some(actual)=out {
            ensure!(Arc::ptr_eq(&actual.client,self),"original runtime observation client replaced");
        } else {*out=Some(RuntimeNativeUseObservation {client:self.clone(),uses:self.uses.observation()});}
        Ok(())
    }
    /// Reserve before the original preparation spawn/queue. This is custody,
    /// not permission to retire the runtime or release immutable source pins.
    pub fn reserve_original_preparation_retaining(&self,out:&mut Option<RuntimePreparationUse>)->Result<()> {
        self.uses.preparation_retaining(out)
    }
    pub fn compiler_sdk_client(&self)->Result<Option<Arc<CompilerSdkClient>>>{self.current()?;let slot=self.compiler_sdk.try_lock().map_err(|_|anyhow::anyhow!("original SDK alias temporarily borrowed"))?;
        if let Some(sdk)=&*slot{sdk.assert_current()?;}Ok(slot.clone())}
    pub(super) fn assert_current(&self)->Result<()>{self.current()}
    fn current(&self)->Result<()>{ensure!(unsafe{libc::prctl(libc::PR_GET_DUMPABLE,0,0,0,0)}==0,"runtime original engine descriptor protection changed");storage::floor(std::process::id(),1500)?;
        self.registration.assert_current()?;self.config.assert_current()?;self.service.live()?;self.service_image.matches_fd(&self.service_exe)?;Ok(())}
    /// OFF App. Known contention is false/outNone before any request/FD send.
    /// Retry uses the SAME caller budget and actual pending start barrier.
    pub fn try_begin_helper_retaining(self:&Arc<Self>,out:&mut Option<RuntimeHelperGrant>,barrier:&ManagedExecutionStartBarrier,original:&FinalizationBudget)->Result<bool>{
        ensure!(out.is_none(),"runtime helper output occupied");original.remaining()?;self.current()?;ensure!(barrier.is_pending(),"runtime enrollment requires original closed start barrier");barrier.require_runtime_image_before_release();
        let (actual_original,cancelled)=barrier.original_request_retaining()?;
        ensure!(actual_original.constraint()==original.constraint(),"runtime enrollment cannot replace original task clock");
        let mut client=match self.state.try_lock(){Ok(v)=>v,Err(TryLockError::WouldBlock)=>return Ok(false),Err(TryLockError::Poisoned(_))=>anyhow::bail!("runtime original client ownership poisoned")};
        if client.active.is_some()||client.backing_active.is_some(){return Ok(false)}ensure!(client.retained.len()<MAX_HELPERS,"runtime retained helper/session bound reached; actual drain required");
        let state=Arc::new(Mutex::new(ClientRequest{original:actual_original,cancelled,helper:barrier.pending_helper_process(),binding:barrier.binding_data().clone(),request:None,bytes:None,pidfd:None,delivery:None,receive:None,cancel:None,pid:0,start:String::new(),sent:false,unknown:false,pending:None,ready:false,attached:false}));
        self.uses.helper(&state)?;
        client.active=Some(Arc::clone(&state));client.retained.push(Arc::clone(&state));*out=Some(RuntimeHelperGrant{client:Arc::clone(self),state});
        let mut state=out.as_ref().unwrap().state.lock().map_err(|_|anyhow::anyhow!("runtime helper owner poisoned"))?;
        state.pidfd=Some(state.helper.duplicate_live_pidfd_for_seed_enrollment()?);let process=Process::capture(state.pidfd.as_ref().unwrap().try_clone()?)?;
        state.pid=process.pid;state.start=original_stat(process.pid)?.1;let(delivery,receive)=wire::runtime_socket_pair()?;state.delivery=Some(delivery);state.receive=Some(receive);state.cancel=Some(cancel_fd()?);
        let reg=self.registration.data();state.request=Some(HelperRequest{version:1,request_nonce:uuid::Uuid::new_v4().to_string(),binding_sha256:digest(&canonical_json(&state.binding)?),engine_instance_id:reg.engine_instance_id.clone(),epoch:reg.binding.epoch.clone(),constraint:original.constraint()});
        state.bytes=Some(canonical_json(state.request.as_ref().unwrap())?);original.remaining()?;self.current()?;Ok(true)
    }
    /// OFF App: propagate actual original cancellation/deadline to the held
    /// kernel eventfd. This closes new helper binding only; no reader capacity,
    /// mount, actor, worker or source owner is released by elapsed time.
    pub(super) fn poll_original_cancellations_retaining(&self)->Result<()> {
        if let Ok(sdk)=self.compiler_sdk.try_lock(){if let Some(sdk)=&*sdk{sdk.poll_cancellations()?;}}
        let client=match self.state.try_lock(){Ok(v)=>v,Err(TryLockError::WouldBlock)=>return Ok(()),Err(TryLockError::Poisoned(_))=>anyhow::bail!("runtime original cancellation owner poisoned")};
        if let Some(backing)=&client.backing_retained{if let Ok(state)=backing.try_lock(){state.signal_original_cancellation()?;}}
        for request in &client.retained{let state=match request.try_lock(){Ok(v)=>v,Err(TryLockError::WouldBlock)=>continue,Err(TryLockError::Poisoned(_))=>anyhow::bail!("runtime original helper owner poisoned")};
            if state.cancelled.load(Ordering::Acquire)||state.original.remaining().is_err(){state.original.cancel();if let Some(cancel)=&state.cancel{signal_cancel(cancel)?;}}
        }Ok(())
    }
    /// Fixed approved DATA projections only, used instead of host runtime paths
    /// by the trusted task resource builder when this actual client is selected.
    pub fn projections_data(&self)->Result<Vec<rudder_engine::execution::ReadOnlyRuntime>>{self.current()?;let mut all:Vec<_>=COMPONENTS.iter().map(|(_,source,guest,_)|rudder_engine::execution::ReadOnlyRuntime{source:PathBuf::from(source),guest_path:(*guest).into()}).collect();
        if self.ready.compiler_sdk_data()?.is_some(){all.push(rudder_engine::execution::ReadOnlyRuntime{source:PathBuf::from(rudder_engine::execution::compiler_sdk::SDK_SOURCE),guest_path:rudder_engine::execution::compiler_sdk::SDK_GUEST.into()});}Ok(all)}
    /// Complete SDK content identities from the original sealed Root ACK and
    /// this SAME live client. Callers retain the client/grant, never these DATA.
    pub fn compiler_toolchains_data(&self)->Result<Option<BTreeMap<String,rudder_engine::build::ToolchainIdentity>>>{self.current()?;
        let Some(sdk)=self.ready.compiler_sdk_data()?else{return Ok(None)};sdk.mount.validate_data()?;
        ensure!(self.config.data().compiler_sdk.as_ref()==Some(&sdk.mount.catalog),"actual SDK client/config origin differs");Ok(Some(sdk.mount.toolchains.clone()))}
}
impl RuntimeHelperGrant {
    /// Identity for a fixed whole-completion observer. The caller must retain
    /// that real carrier; this token alone cannot certify task settlement.
    pub fn observe_original_start(&self,client:&Arc<RuntimeImageClient>,barrier:&ManagedExecutionStartBarrier)->Result<RuntimeHelperUseIdentity> {
        ensure!(Arc::ptr_eq(client,&self.client),"observed runtime start changed original client");
        self.client.uses.try_contains_helper(&self.state)?;
        let state=self.state.try_lock().map_err(|_|anyhow::anyhow!("observed original runtime helper busy/poisoned"))?;
        ensure!(state.ready&&state.attached&&!state.unknown&&!barrier.is_pending()
            &&barrier.binding_data()==&state.binding&&Arc::ptr_eq(&barrier.pending_helper_process(),&state.helper),
            "observed runtime start differs from original attached helper grant");
        Ok(RuntimeHelperUseIdentity {client:self.client.clone(),state:self.state.clone()})
    }
    /// Exact retained-start identity only, after the binary's genuine whole
    /// completion checks. Do not require a live helper or renew its old clock:
    /// the original successful helper exit remains separately owned by caller.
    pub fn check_original_start(&self,client:&Arc<RuntimeImageClient>,barrier:&ManagedExecutionStartBarrier)->Result<()> {
        ensure!(Arc::ptr_eq(client,&self.client),"joined runtime start changed original client");
        self.client.uses.contains_helper(&self.state)?;
        let state=self.state.try_lock().map_err(|_|anyhow::anyhow!("original runtime helper custody busy/poisoned"))?;
        ensure!(state.ready&&state.attached&&!state.unknown&&!barrier.is_pending()
            &&barrier.binding_data()==&state.binding&&Arc::ptr_eq(&barrier.pending_helper_process(),&state.helper),
            "joined runtime start differs from original attached helper grant");Ok(())
    }

    pub fn poll_retaining(&mut self)->Result<bool>{let mut client=match self.client.state.try_lock(){Ok(v)=>v,Err(TryLockError::WouldBlock)=>return Ok(false),Err(TryLockError::Poisoned(_))=>anyhow::bail!("runtime client owner poisoned")};
        let mut state=self.state.lock().map_err(|_|anyhow::anyhow!("runtime helper owner poisoned"))?;
        if state.ready{return Ok(true)}ensure!(!state.unknown&&client.active.as_ref().is_some_and(|a|Arc::ptr_eq(a,&self.state)),"runtime request unknown or belongs to another active owner");
        let result=(||->Result<bool>{if state.cancelled.load(Ordering::Acquire){state.original.cancel();}state.original.remaining()?;self.client.current()?;let _held=state.helper.duplicate_live_pidfd_for_seed_enrollment()?;
            if !state.sent{match client.endpoint.send_data(state.bytes.as_ref().context("runtime request capture incomplete")?,&[state.pidfd.as_ref().unwrap(),state.delivery.as_ref().unwrap(),state.cancel.as_ref().unwrap()]){Ok(false)=>return Ok(false),Ok(true)=>state.sent=true,Err(e)=>return Err(e)}}
            if state.pending.is_none(){state.pending=client.endpoint.receive_data()?;}let Some(packet)=&state.pending else{return Ok(false)};
            ensure!(packet.credentials.pid==self.client.service.pid as i32&&packet.credentials.uid==0&&packet.credentials.gid==0&&packet.fds.is_empty(),"runtime enrollment ACK not original Root service");
            let ack:HelperAck=serde_json::from_slice(&packet.bytes)?;let request=state.request.as_ref().unwrap();
            ensure!(ack.version==1&&ack.request_nonce==request.request_nonce&&ack.binding_sha256==request.binding_sha256&&ack.engine_instance_id==request.engine_instance_id&&ack.epoch==request.epoch
                &&ack.helper_pid==state.pid&&ack.helper_start_ticks==state.start,"runtime original helper enrollment ACK differs");
            state.original.remaining()?;self.client.current()?;let _held=state.helper.duplicate_live_pidfd_for_seed_enrollment()?;state.ready=true;Ok(true)})();
        match result{Ok(true)=>{client.active=None;Ok(true)},Ok(false)=>Ok(false),Err(e)=>{state.unknown=true;if let Some(cancel)=&state.cancel{let _=signal_cancel(cancel);}Err(e)}}
    }
    /// Only after the original timely Root ACK. Root and machine client retain
    /// their originals; this duplicate goes into the same staged handoff cell.
    pub fn attach_to_original_barrier_retaining(&mut self,barrier:&mut ManagedExecutionStartBarrier)->Result<()> {
        self.client.current()?;let mut state=self.state.lock().map_err(|_|anyhow::anyhow!("runtime helper owner poisoned"))?;state.original.remaining()?;
        let (actual_original,cancelled)=barrier.original_request_retaining()?;ensure!(actual_original.constraint()==state.original.constraint()&&Arc::ptr_eq(&cancelled,&state.cancelled),"runtime attachment replaced actual task clock/cancellation carrier");
        ensure!(state.ready&&!state.unknown&&!state.attached&&barrier.is_pending()&&barrier.binding_data()==&state.binding
            &&Arc::ptr_eq(&barrier.pending_helper_process(),&state.helper),"runtime attachment changed original helper/barrier");
        let fd=state.receive.as_ref().context("runtime original receive endpoint absent")?.try_clone()?;
        let attachment=RuntimeImageHandoffAttachment::from_original_endpoint(fd)?;barrier.attach_runtime_image_retaining(attachment)?;state.attached=true;
        state.original.remaining()?;Ok(())
    }
}

#[cfg(test)]
#[path="root_runtime_image_tests.rs"]
mod tests;

#[cfg(test)]
#[path="root_runtime_inherited_tests.rs"]
mod inherited_tests;

#[cfg(test)]
#[path = "root_runtime_protection_tests.rs"]
mod protection_tests;

#[cfg(test)]
#[path = "root_runtime_prepare_diagnostic_tests.rs"]
mod prepare_diagnostic_tests;

#[cfg(test)]
mod bootstrap_diagnostic_tests {
    use super::*;

    #[test]
    fn bootstrap_check_exposes_only_typed_fixed_label() {
        let error = proof_check(BootstrapCheck::EngineIdMaps, || -> Result<()> {
            Err(std::io::Error::from_raw_os_error(libc::EACCES))
                .context("private-config-and-credential")
        })
        .unwrap_err()
        .context("private-outer-path");
        assert_eq!(
            bootstrap_failure_check(&error).as_deref(),
            Some("EngineIdMaps")
        );
        assert_eq!(
            error
                .chain()
                .find_map(|cause| cause.downcast_ref::<std::io::Error>())
                .unwrap()
                .raw_os_error(),
            Some(libc::EACCES)
        );
        assert!(
            bootstrap_failure_check(&anyhow::anyhow!("EngineIdMaps private credential")).is_none()
        );
        assert_eq!(proof_check(BootstrapCheck::Boot, || Ok(7)).unwrap(), 7);
    }
}


#[cfg(test)]
#[path = "root_runtime_component_diagnostic_tests.rs"]
mod component_diagnostic_tests;
