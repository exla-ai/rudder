//! Original Root registration only. MAC authenticates endpoint evidence; it
//! grants no project/source/task/NET permission and never reconstructs custody.
#![cfg(target_os="linux")]
use anyhow::{ensure,Context,Result};
use hmac::{Hmac,Mac};
use sha2::{Digest,Sha256};
use serde::{Serialize,Deserialize};
use crate::root_storage_service::{self as storage,Image,Process};
use super::host_ingress::{HostIngressConfig,HostIngressConfigOwner,RootHostControl};
use rudder_engine::{execution::finalization_budget::{FinalizationBudget,FinalizationConstraint},protocol::canonical_json};
use std::{collections::BTreeMap,fs::{self,File,OpenOptions},os::{fd::{AsRawFd,OwnedFd},unix::fs::{FileExt,MetadataExt,OpenOptionsExt}},
    path::{Path,PathBuf},sync::{Arc,atomic::{AtomicBool,Ordering}}};
#[path="host_transport_root.rs"]
mod host_transport_root;
#[path="engine_operations.rs"]
pub mod engine_operations;
pub use engine_operations::{EngineOperation,EngineOperationKind,EngineOperationAck,EngineOperationError,FreshProjectPreparation,FreshPreparationData,ImagePreseedBudget,ImagePreseedConstraint,RootUploadTransfer,MissingObjectsAck,UploadScope,OperationConstraint,PublishedImagePreseed,OriginalFreshAcquireAck,OriginalOwnerRenewalAck};

const MAX:usize=16*1024;
const KEY:&str="/run/rudder-host-control/registration-key";
const POLICY:&str="/usr/local/share/rudder-managed-image/sidecar-policy-pin.json";
const RUNTIME:&str="/opt/rudder-managed-runtime/v1/manifest.json";
const BEGIN:&[u8]=b"rudder_engine_registration_begin_v1\0";
const CHALLENGE:&[u8]=b"rudder_engine_registration_challenge_v1\0";
const PROOF:&[u8]=b"rudder_engine_registration_proof_v1\0";
const ACK:&[u8]=b"rudder_engine_registration_ack_v1\0";
const CREDENTIAL:&[u8]=b"rudder_engine_registration_credential_v1\0";
fn fault()->anyhow::Error {anyhow::anyhow!("engine registration unavailable; original owners retained")}
struct Secret<const N:usize>([u8;N]);
impl<const N:usize> Drop for Secret<N> {fn drop(&mut self){for value in &mut self.0 {unsafe {std::ptr::write_volatile(value,0);}}
    std::sync::atomic::compiler_fence(Ordering::SeqCst);}}
fn hex(bytes:&[u8])->String {bytes.iter().map(|b|format!("{b:02x}")).collect()}
fn unhex<const N:usize>(text:&[u8])->Result<[u8;N]> {ensure!(text.len()==N*2,"registration fixed hex length invalid");let mut out=[0;N];
    fn digit(b:u8)->Result<u8>{match b {b'0'..=b'9'=>Ok(b-b'0'),b'a'..=b'f'=>Ok(b-b'a'+10),_=>Err(fault())}}
    for i in 0..N {out[i]=digit(text[2*i])?*16+digit(text[2*i+1])?;}Ok(out)}
fn uuid(value:&str)->Result<()> {ensure!(uuid::Uuid::parse_str(value).is_ok_and(|v|v.get_version_num()==4&&v.to_string()==value),"registration UUID invalid");Ok(())}
fn bytes<T:Serialize>(value:&T)->Result<Vec<u8>> {let b=canonical_json(value).map_err(|_|fault())?;ensure!(b.len()<=MAX,"registration packet exceeds bound");Ok(b)}
#[derive(Clone,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Signed<T> {body:T,mac:String}
fn sign<T:Serialize>(key:&[u8],domain:&[u8],body:&T)->Result<String>{let mut mac=<Hmac<Sha256> as Mac>::new_from_slice(key).map_err(|_|fault())?;
    mac.update(domain);mac.update(&bytes(body)?);Ok(hex(&mac.finalize().into_bytes()))}
fn verify<T:Serialize>(key:&[u8],domain:&[u8],packet:&Signed<T>)->Result<()> {let tag=unhex::<32>(packet.mac.as_bytes())?;
    let mut mac=<Hmac<Sha256> as Mac>::new_from_slice(key).map_err(|_|fault())?;mac.update(domain);mac.update(&bytes(&packet.body)?);
    mac.verify_slice(&tag).map_err(|_|fault())}
#[derive(Clone,PartialEq,Eq)]
struct Stamp {device:u64,inode:u64,len:u64,ctime:(i64,i64),mtime:(i64,i64)}
impl Stamp {fn capture(file:&File,mode:u32)->Result<Self>{let m=file.metadata()?;ensure!(m.is_file()&&m.uid()==0&&m.gid()==0&&m.nlink()==1&&m.mode()&0o7777==mode,"registration protected file invalid");
    Ok(Self {device:m.dev(),inode:m.ino(),len:m.len(),ctime:(m.ctime(),m.ctime_nsec()),mtime:(m.mtime(),m.mtime_nsec())})}}
struct ProtectedFile {path:PathBuf,file:File,stamp:Stamp,mode:u32}
impl ProtectedFile {
    fn capture(path:&str,mode:u32,max:usize)->Result<Self>{let path=PathBuf::from(path);ensure!(path.canonicalize()?==path,"registration file redirects");
        for parent in path.parent().context("registration file parent absent")?.ancestors(){let m=fs::symlink_metadata(parent)?;
            ensure!(m.is_dir()&&!m.file_type().is_symlink()&&m.uid()==0&&m.gid()==0&&m.mode()&0o022==0,"registration file ancestor changed");}
        let file=OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW|libc::O_CLOEXEC|libc::O_NONBLOCK).open(&path)?;let stamp=Stamp::capture(&file,mode)?;
        ensure!(stamp.len>0&&stamp.len<=max as u64,"registration file bound invalid");let value=Self {path,file,stamp,mode};value.current()?;Ok(value)}
    fn current(&self)->Result<()> {
        for parent in self.path.parent().context("registration original ancestor absent")?.ancestors(){let m=fs::symlink_metadata(parent)?;
            ensure!(m.is_dir()&&!m.file_type().is_symlink()&&m.uid()==0&&m.gid()==0&&m.mode()&0o022==0,"registration original ancestor changed");}
        ensure!(Stamp::capture(&self.file,self.mode)?==self.stamp,"registration original file changed");let m=fs::symlink_metadata(&self.path)?;
        ensure!(!m.file_type().is_symlink()&&(m.dev(),m.ino())==(self.stamp.device,self.stamp.inode),"registration file name changed");Ok(())}
    fn hash(&self)->Result<String>{self.current()?;let mut data=vec![0;self.stamp.len as usize];ensure!(self.file.read_at(&mut data,0)?==data.len(),"registration file read incomplete");self.current()?;Ok(hex(&Sha256::digest(data)))}
}
struct RootKey {file:ProtectedFile,key:Secret<32>}
impl RootKey {
    fn capture()->Result<Self>{ensure!(unsafe {libc::getuid()}==0&&unsafe {libc::geteuid()}==0,"registration signing key requires actual Root");
        let file=ProtectedFile::capture(KEY,0o600,65)?;ensure!(file.stamp.len==65,"registration key encoding invalid");
        let mut raw=Secret([0u8;65]);ensure!(file.file.read_at(&mut raw.0,0)?==65&&raw.0[64]==b'\n',"registration key encoding invalid");
        let mut key=Secret([0u8;32]);for i in 0..32 {fn digit(b:u8)->Result<u8>{match b {b'0'..=b'9'=>Ok(b-b'0'),b'a'..=b'f'=>Ok(b-b'a'+10),_=>Err(fault())}}
            key.0[i]=digit(raw.0[2*i])?*16+digit(raw.0[2*i+1])?;}file.current()?;Ok(Self {file,key})}
}
#[derive(Clone,Debug,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistrationBinding {pub account_id:String,pub home_id:String,pub app_id:String,pub app_name:String,pub network_name:String,pub network_cidr:String,
    pub machine_id:String,pub instance_id:String,pub image:String,pub config_sha256:String,pub machine_config_sha256:String,pub store_id:String,pub incarnation:String,
    pub provision_nonce:String,pub epoch:String,pub native_sha256:String,pub launcher_sha256:String,pub service_sha256:String,pub node_sha256:String,
    pub sidecar_policy_sha256:String,pub runtime_manifest_sha256:String}
#[derive(Clone,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct BeginBody {version:u32,kind:String,home_id:String,incarnation:String,config_sha256:String,root_nonce:String,constraint:FinalizationConstraint}
#[derive(Clone,Debug,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct ChallengeBody {version:u32,kind:String,challenge_id:String,challenge_nonce:String,root_nonce:String,engine_instance_id:String,credential_id:String,
    binding:RegistrationBinding,constraint:FinalizationConstraint}
#[derive(Clone,Debug,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct ObservedId {device:String,inode:String}
#[derive(Clone,Debug,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct ProcessEvidence {pid:u32,start_ticks:String,image_sha256:String,exe:ObservedId,namespaces:BTreeMap<String,ObservedId>}
#[derive(Clone,Debug,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Evidence {boot_id:String,root:ProcessEvidence,engine:ProcessEvidence,service:ProcessEvidence,host_worker_tid:u32,host_worker_start_ticks:String,
    root_client_authenticated:bool,parent_authentication_accepted:bool,host_worker_ack_authenticated:bool}
#[derive(Clone,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct ProofBody {version:u32,kind:String,challenge_id:String,challenge_nonce:String,root_nonce:String,engine_instance_id:String,credential_id:String,
    binding:RegistrationBinding,constraint:FinalizationConstraint,evidence:Evidence}
#[derive(Clone,Debug,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistrationAck {pub version:u32,pub kind:String,pub challenge_id:String,pub challenge_nonce:String,pub root_nonce:String,pub engine_instance_id:String,
    pub credential_id:String,pub binding:RegistrationBinding,pub constraint:FinalizationConstraint,pub boot_id:String,pub proof_sha256:String}
#[derive(Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Relay<T>{kind:String,rpc_id:String,packet:T}
#[derive(Clone,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Certificate {version:u32,root_nonce:String,constraint:FinalizationConstraint,ack:RegistrationAck,worker_tid:u32,worker_start_ticks:String}
// Cryptographic matching DATA only. This helper cannot create an owner or
// attest a process. The actual Root phase retains packet before calling it.
fn ack_matches_proof(a:&RegistrationAck,p:&ProofBody)->Result<()> {
    ensure!(a.version==1&&a.kind=="ack"&&a.challenge_id==p.challenge_id&&a.challenge_nonce==p.challenge_nonce&&a.root_nonce==p.root_nonce
        &&a.engine_instance_id==p.engine_instance_id&&a.credential_id==p.credential_id&&a.binding==p.binding&&a.constraint==p.constraint&&a.boot_id==p.evidence.boot_id
        &&a.proof_sha256==hex(&Sha256::digest(bytes(p)?)),"registration ACK differs from original proof");Ok(())
}
fn identity(file:&File)->Result<ObservedId>{let (device,inode)=storage::id(file)?;Ok(ObservedId {device:device.to_string(),inode:inode.to_string()})}
fn start_ticks(data:&[u8],pid:u32)->Result<String>{let text=std::str::from_utf8(data).map_err(|_|fault())?;let (left,right)=text.rsplit_once(") ").context("registration proc stat invalid")?;
    ensure!(left.split_whitespace().next()==Some(pid.to_string().as_str()),"registration proc PID differs");let value=right.split_whitespace().nth(19).context("registration start time absent")?;
    ensure!(!value.is_empty()&&value.bytes().all(|b|b.is_ascii_digit()),"registration start time invalid");Ok(value.into())}
fn read_stat(file:&File,pid:u32)->Result<String>{let mut data=[0u8;4096];let n=file.read_at(&mut data,0)?;ensure!(n>0&&n<data.len(),"registration retained stat unavailable");start_ticks(&data[..n],pid)}
fn namespaces(pid:u32)->Result<Vec<(String,File,i32)>> {[("mnt",libc::CLONE_NEWNS),("user",libc::CLONE_NEWUSER),("net",libc::CLONE_NEWNET),("pid",libc::CLONE_NEWPID)].into_iter()
    .map(|(name,kind)|Ok((name.into(),storage::namespace(Some(pid),name,kind)?,kind))).collect()}
struct ObservedProcess {process:Process,image_sha:String,exe:File,exe_stamp:Stamp,exe_mode:u32,stat:File,start:String,ns:Vec<(String,File,i32)>}
impl ObservedProcess {
    fn capture(process:&Process,image:&Image)->Result<Self>{image.matches_process(process)?;let process=Process::capture(process.fd().try_clone()?)?;
        let exe=File::open(format!("/proc/{}/exe",process.pid))?;let duplicate:OwnedFd=exe.try_clone()?.into();image.matches_fd(&duplicate)?;
        let exe_mode=exe.metadata()?.mode()&0o7777;ensure!(exe_mode&0o022==0&&exe_mode&0o005==0o005,"registration actual image mode invalid");let exe_stamp=Stamp::capture(&exe,exe_mode)?;
        let stat=OpenOptions::new().read(true).custom_flags(libc::O_CLOEXEC|libc::O_NOFOLLOW).open(format!("/proc/{}/stat",process.pid))?;
        let start=read_stat(&stat,process.pid)?;let ns=namespaces(process.pid)?;let value=Self {process,image_sha:image.sha256.clone(),exe,exe_stamp,exe_mode,stat,start,ns};value.current()?;image.matches_process(&value.process)?;Ok(value)}
    fn current(&self)->Result<()> {self.process.live()?;ensure!(Stamp::capture(&self.exe,self.exe_mode)?==self.exe_stamp&&storage::id(&self.exe)?==storage::id(&File::open(format!("/proc/{}/exe",self.process.pid))?)?
        && read_stat(&self.stat,self.process.pid)?==self.start,"registration original process changed");
        for (name,file,kind) in &self.ns{ensure!(storage::id(file)?==storage::id(&storage::namespace(Some(self.process.pid),name,*kind)?)?
            &&storage::id(file)?==storage::id(&storage::namespace(None,name,*kind)?)?,"registration process left original host namespaces");}Ok(())}
    fn data(&self)->Result<ProcessEvidence>{self.current()?;Ok(ProcessEvidence {pid:self.process.pid,start_ticks:self.start.clone(),image_sha256:self.image_sha.clone(),exe:identity(&self.exe)?,
        namespaces:self.ns.iter().map(|(name,file,_)|Ok((name.clone(),identity(file)?))).collect::<Result<_>>()?})}
}
/// Capture ONLY at the original established Root bootstrap after authenticated
/// client+HostServing ACK. Actual parents retain the original Child/wait owners.
pub(super) struct OriginalEvidence {root:ObservedProcess,engine:ObservedProcess,service:ObservedProcess,worker:File,worker_tid:u32,worker_start:String,boot:Vec<u8>}
impl OriginalEvidence {
    pub(super) fn capture(root:&Process,root_image:&Image,engine:&Process,engine_image:&Image,service:&Process,service_image:&Image,worker_tid:u32)->Result<Self>{
        let root=ObservedProcess::capture(root,root_image)?;let engine=ObservedProcess::capture(engine,engine_image)?;let service=ObservedProcess::capture(service,service_image)?;
        let worker=OpenOptions::new().read(true).custom_flags(libc::O_CLOEXEC|libc::O_NOFOLLOW).open(format!("/proc/{}/task/{worker_tid}/stat",engine.process.pid))?;
        let worker_start=read_stat(&worker,worker_tid)?;let value=Self {root,engine,service,worker,worker_tid,worker_start,boot:storage::proc_bytes("/proc/sys/kernel/random/boot_id",64)?};value.current()?;Ok(value)}
    fn current(&self)->Result<()> {self.root.current()?;self.engine.current()?;self.service.current()?;storage::floor(self.root.process.pid,0)?;storage::floor(self.service.process.pid,0)?;storage::floor(self.engine.process.pid,1500)?;
        ensure!(read_stat(&self.worker,self.worker_tid)?==self.worker_start&&self.boot==storage::proc_bytes("/proc/sys/kernel/random/boot_id",64)?,"registration original worker/boot changed");
        let status=String::from_utf8(storage::proc_bytes(&format!("/proc/{}/task/{}/status",self.engine.process.pid,self.worker_tid),16384)?)?;
        ensure!(super::status_field(&status,"Tgid:")?.parse::<u32>()?==self.engine.process.pid&&super::status_field(&status,"Pid:")?.parse::<u32>()?==self.worker_tid,"registration worker differs");super::zero_caps_and_groups(&status)?;
        for field in ["Uid:","Gid:"]{ensure!(super::observed_ids(&status,field)?==[1500;4],"registration worker credential floor changed");}Ok(())}
    fn data(&self)->Result<Evidence>{self.current()?;Ok(Evidence {boot_id:String::from_utf8(self.boot.clone())?.trim().into(),root:self.root.data()?,engine:self.engine.data()?,service:self.service.data()?,
        host_worker_tid:self.worker_tid,host_worker_start_ticks:self.worker_start.clone(),root_client_authenticated:true,parent_authentication_accepted:true,host_worker_ack_authenticated:true})}
}
#[derive(Clone,Copy,PartialEq,Eq)]
enum Phase {Begin,Challenge,Proof,Ack,NativeOffer,NativeAck,Complete,Failed}
pub(super) struct RootRegistration {original:FinalizationBudget,nonce:String,config:HostIngressConfig,config_sha:String,key:RootKey,policy:ProtectedFile,runtime:ProtectedFile,
    transport:host_transport_root::HostTransportOwner,operations:engine_operations::RootOperationBridge,evidence:Option<OriginalEvidence>,begin:Signed<BeginBody>,begin_wire:Vec<u8>,proof:Option<Signed<ProofBody>>,proof_wire:Option<Vec<u8>>,challenge:Option<Signed<ChallengeBody>>,
    inbound:Option<storage::Packet>,ack:Option<Signed<RegistrationAck>>,ack_verified:bool,derived:Option<Secret<69>>,certificate:Option<OwnedFd>,phase:Phase,
    native_ack_seen:bool,notice:Option<Vec<u8>>,notice_sent:bool,notice_unknown:bool,fence_attempted:bool}
impl RootRegistration {
    pub(super) fn capture(original:&FinalizationBudget,nonce:&str,host:&RootHostControl)->Result<Self>{original.remaining()?;let key=RootKey::capture()?;
        let config=host.registration_config().clone();let config_sha=host.registration_config_sha().to_owned();uuid(nonce)?;
        let body=BeginBody {version:1,kind:"begin".into(),home_id:config.home_id.clone(),incarnation:config.incarnation.clone(),config_sha256:config_sha.clone(),root_nonce:nonce.into(),constraint:original.constraint()};
        let begin=Signed {mac:sign(&key.key.0,BEGIN,&body)?,body};let begin_wire=bytes(&Relay {kind:"engine_registration_begin_v1".into(),rpc_id:nonce.into(),packet:&begin})?;
        let value=Self {original:original.clone(),nonce:nonce.into(),config,config_sha,key,policy:ProtectedFile::capture(POLICY,0o444,64*1024)?,runtime:ProtectedFile::capture(RUNTIME,0o444,64*1024)?,
            transport:host_transport_root::HostTransportOwner::new(),operations:engine_operations::RootOperationBridge::capture()?,evidence:None,begin,begin_wire,proof:None,proof_wire:None,challenge:None,inbound:None,ack:None,ack_verified:false,derived:None,certificate:None,phase:Phase::Begin,
            native_ack_seen:false,notice:None,notice_sent:false,notice_unknown:false,fence_attempted:false};value.original.remaining()?;Ok(value)}
    pub(super) fn needs_evidence(&self)->bool {self.evidence.is_none()}
    pub(super) fn set_evidence_retaining(&mut self,evidence:OriginalEvidence)->Result<()> {ensure!(self.evidence.is_none(),"registration original evidence already staged");self.evidence=Some(evidence);self.current()?;Ok(())}
    fn current(&self)->Result<()> {self.key.file.current()?;self.policy.current()?;self.runtime.current()?;self.evidence.as_ref().context("registration actual evidence absent")?.current()}
    fn binding(&self,b:&RegistrationBinding,node_sha:&str)->Result<()> {ensure!(b.account_id==self.config.account_id&&b.home_id==self.config.home_id&&b.machine_id==self.config.machine_id
        &&b.store_id==self.config.store_id&&b.incarnation==self.config.incarnation&&b.config_sha256==self.config_sha&&b.runtime_manifest_sha256==self.config.runtime_manifest.manifest_sha256
        &&b.runtime_manifest_sha256==self.runtime.hash()?&&b.sidecar_policy_sha256==self.policy.hash()?&&b.node_sha256==node_sha&&b.epoch=="1","registration binding differs from original provisioned image/config");
        if let Some(p)=&self.config.image_preseed {ensure!(p.image==b.image,"registration qualified image preseed policy differs");}
        let e=self.evidence.as_ref().context("registration original evidence absent")?.data()?;
        ensure!(e.root.image_sha256==b.launcher_sha256&&e.engine.image_sha256==b.native_sha256&&e.service.image_sha256==b.service_sha256,"registration actual executable differs from challenge");Ok(())}
    pub(super) fn poll_retaining(&mut self,host:&mut RootHostControl)->Result<()> {let result=(||->Result<()> {
        if self.phase==Phase::Failed{return Ok(());}self.current()?;
        if self.phase==Phase::Complete{return Ok(());}self.original.remaining()?;
        match self.phase {
            Phase::Begin=>{if host.registration_send(&self.begin_wire)?{self.phase=Phase::Challenge;self.original.remaining()?;}},
            Phase::Challenge=>{host.registration_receive_retaining(&mut self.inbound)?;if self.inbound.is_some(){
                let relay:Relay<Signed<ChallengeBody>>=serde_json::from_slice(&self.inbound.as_ref().unwrap().bytes).map_err(|_|fault())?;
                self.challenge=Some(relay.packet);let packet=self.challenge.as_ref().unwrap();verify(&self.key.key.0,CHALLENGE,packet)?;let c=&packet.body;
                ensure!(relay.kind=="engine_registration_challenge_v1"&&relay.rpc_id==self.nonce&&c.version==1&&c.kind=="challenge"&&c.root_nonce==self.nonce&&c.constraint==self.original.constraint(),"registration challenge original binding differs");
                uuid(&c.challenge_id)?;uuid(&c.engine_instance_id)?;uuid(&c.credential_id)?;unhex::<32>(c.challenge_nonce.as_bytes())?;self.binding(&c.binding,host.registration_node_sha())?;
                self.original.remaining()?;let body=ProofBody {version:1,kind:"proof".into(),challenge_id:c.challenge_id.clone(),challenge_nonce:c.challenge_nonce.clone(),root_nonce:c.root_nonce.clone(),
                    engine_instance_id:c.engine_instance_id.clone(),credential_id:c.credential_id.clone(),binding:c.binding.clone(),constraint:c.constraint.clone(),evidence:self.evidence.as_ref().unwrap().data()?};
                self.proof=Some(Signed {mac:sign(&self.key.key.0,PROOF,&body)?,body});self.proof_wire=Some(bytes(&Relay {kind:"engine_registration_proof_v1".into(),rpc_id:self.nonce.clone(),packet:self.proof.as_ref().unwrap()})?);
                self.inbound.take();self.phase=Phase::Proof;
            }},
            Phase::Proof=>{if host.registration_send(self.proof_wire.as_ref().unwrap())?{self.phase=Phase::Ack;self.original.remaining()?;}},
            Phase::Ack=>{host.registration_receive_retaining(&mut self.inbound)?;if self.inbound.is_some(){self.adopt_ack_retaining()?;self.original.remaining()?;self.phase=Phase::NativeOffer;}},
            Phase::NativeOffer|Phase::NativeAck=>{},Phase::Complete|Phase::Failed=>{},
        }self.current()?;Ok(())})();if result.is_err(){self.phase=Phase::Failed;}result}
    fn adopt_ack_retaining(&mut self)->Result<()> {let relay:Relay<Signed<RegistrationAck>>=serde_json::from_slice(&self.inbound.as_ref().context("registration original ACK packet absent")?.bytes).map_err(|_|fault())?;
        self.ack=Some(relay.packet);let ack=self.ack.as_ref().unwrap();verify(&self.key.key.0,ACK,ack)?;let p=&self.proof.as_ref().context("registration original proof absent")?.body;let a=&ack.body;
        ensure!(relay.kind=="engine_registration_ack_v1"&&relay.rpc_id==self.nonce,"registration ACK correlation differs");ack_matches_proof(a,p)?;
        // Retain known ACK BEFORE the original deadline guard; a late known
        // registration remains recovery DATA, never a Native owner factory.
        self.ack_verified=true;self.current()?;self.original.remaining()?;
        #[derive(Serialize)]struct Derive<'a>{challenge_id:&'a str,engine_instance_id:&'a str,credential_id:&'a str,root_nonce:&'a str}
        let d=Derive {challenge_id:&a.challenge_id,engine_instance_id:&a.engine_instance_id,credential_id:&a.credential_id,root_nonce:&a.root_nonce};
        let mut mac=<Hmac<Sha256> as Mac>::new_from_slice(&self.key.key.0).map_err(|_|fault())?;mac.update(CREDENTIAL);mac.update(&bytes(&d)?);let value=Secret::<32>(mac.finalize().into_bytes().into());
        let mut derived=Secret([0u8;69]);derived.0[..5].copy_from_slice(b"rdre_");const HEX:&[u8]=b"0123456789abcdef";
        for (i,b) in value.0.iter().copied().enumerate(){derived.0[5+2*i]=HEX[(b>>4)as usize];derived.0[6+2*i]=HEX[(b&15)as usize];}self.derived=Some(derived);Ok(())}
    pub(super) fn certificate_retaining(&mut self)->Result<Option<&OwnedFd>>{if self.phase!=Phase::NativeOffer{return Ok(None);}self.original.remaining()?;self.current()?;
        if self.certificate.is_none(){let p=&self.proof.as_ref().unwrap().body;let c=Certificate {version:1,root_nonce:self.nonce.clone(),constraint:self.original.constraint(),ack:self.ack.as_ref().unwrap().body.clone(),worker_tid:p.evidence.host_worker_tid,worker_start_ticks:p.evidence.host_worker_start_ticks.clone()};
            self.certificate=Some(super::sealed_bytes(&bytes(&c)?)?);}Ok(self.certificate.as_ref())}
    pub(super) fn settle_uploaders_retaining(&mut self)->Result<()> {self.operations.settle_uploaders_retaining()}
    pub(super) fn operations_native_fd(&self)->&OwnedFd {self.operations.native_fd()}
    pub(super) fn poll_transport_retaining(&mut self,host:&mut RootHostControl)->Result<()> {
        ensure!(self.complete(),"transport replacement requires actual original registered Native acceptance");self.current()?;
        let pending=self.operations.original_pending_ack_bindings()?;
        self.transport.poll_retaining(host,&self.ack.as_ref().unwrap().body,&self.derived.as_ref().context("original Root-only credential absent")?.0,&self.evidence.as_ref().unwrap().engine.process,&self.original,&pending)?;self.current()
    }
    pub(super) fn poll_operations_retaining(&mut self,host:&mut RootHostControl)->Result<()> {
        ensure!(self.phase==Phase::Complete,"engine operations require actual registered Native acceptance");self.current()?;
        self.operations.poll_retaining(host,&self.ack.as_ref().unwrap().body,&self.derived.as_ref().context("original Root engine credential absent")?.0,
            &self.evidence.as_ref().unwrap().engine.process,&self.config)
    }
    pub(super) fn reconcile_operation_retaining(&mut self,host:&mut RootHostControl)->Result<bool> {
        self.operations.reconcile_only_retaining(host,&self.ack.as_ref().context("original registration ACK absent")?.body,
            &self.derived.as_ref().context("original Root credential absent")?.0)
    }
    pub(super) fn native_offered(&mut self){self.phase=Phase::NativeAck;}
    pub(super) fn native_accepted_retaining(&mut self)->Result<()> {self.native_ack_seen=true;ensure!(self.phase==Phase::NativeAck,"registration Native ACK not expected");self.current()?;self.original.remaining()?;
        self.notice=Some(bytes(&serde_json::json!({"kind":"engine_registration_complete_v1","challenge_id":self.ack.as_ref().unwrap().body.challenge_id}))?);self.phase=Phase::Complete;Ok(())}
    pub(super) fn flush_complete(&mut self,host:&mut RootHostControl)->Result<()> {if self.phase==Phase::Complete&&!self.notice_sent&&!self.notice_unknown {self.current()?;
        match host.registration_send(self.notice.as_ref().unwrap()) {Ok(true)=>self.notice_sent=true,Ok(false)=>{},Err(_)=>{self.notice_unknown=true;return Err(fault());}}}Ok(())}
    pub(super) fn complete(&self)->bool {self.phase==Phase::Complete&&self.notice_sent}
    pub(super) fn failed(&self)->bool {self.phase==Phase::Failed}
    pub(super) fn mark_failed(&mut self){self.phase=Phase::Failed;}
    pub(super) fn fence_once(&mut self)->bool {if self.fence_attempted{return false;}self.fence_attempted=true;true}
    /// Explicit inspect-only late-ACK reconciliation, no resend/new deadline or
    /// Native certificate. Actual Root owns the original endpoint and proof.
    pub(super) fn reconcile_late_ack_retaining(&mut self,host:&mut RootHostControl)->Result<bool>{ensure!(self.phase==Phase::Failed,"registration not failed");
        if self.inbound.is_none(){host.registration_receive_settled_retaining(&mut self.inbound)?;}if self.inbound.is_none(){return Ok(false);}
        let _=self.adopt_ack_retaining();Ok(self.ack_verified)}
}

/// Actual original Root-channel ACK + current physical engine/Root/service
/// custody only. Not a PG lease, source/task capability or live remote status.
pub struct RegisteredEngineOwner {operations:std::sync::Mutex<engine_operations::NativeChannel>,data:RegistrationAck,certificate:File,config:Arc<HostIngressConfigOwner>,own:Process,root:Process,service:Process,
    image:Image,service_image:Image,service_exe:OwnedFd,root_image:Image,root_exe:OwnedFd,worker:File,worker_tid:u32,worker_start:String,
    ns:Vec<(String,File,i32)>,boot:Vec<u8>,fenced:Arc<AtomicBool>}
impl RegisteredEngineOwner {
    pub fn engine_id(&self)->&str {&self.data.engine_instance_id}pub fn boot_id(&self)->&str {&self.data.boot_id}
    pub fn account_id(&self)->&str {&self.data.binding.account_id}pub fn home_id(&self)->&str {&self.data.binding.home_id}
    pub fn machine_id(&self)->&str {&self.data.binding.machine_id}pub fn incarnation(&self)->&str {&self.data.binding.incarnation}
    pub fn credential_id(&self)->&str {&self.data.credential_id}pub fn epoch(&self)->&str {&self.data.binding.epoch}
    pub fn data(&self)->&RegistrationAck {&self.data}
    pub fn assert_current(&self)->Result<()> {let result=(||->Result<()>{ensure!(!self.fenced.load(Ordering::Acquire),"registered original engine fenced");self.config.assert_current()?;
        ensure!(self.own.pid==std::process::id(),"registered engine moved process");self.own.live()?;self.root.live()?;self.service.live()?;storage::floor(self.own.pid,1500)?;storage::floor(self.root.pid,0)?;
        self.image.matches_process(&self.own)?;self.service_image.check()?;self.service_image.matches_fd(&self.service_exe)?;self.root_image.check()?;self.root_image.matches_fd(&self.root_exe)?;
        ensure!(read_stat(&self.worker,self.worker_tid)?==self.worker_start&&self.boot==storage::proc_bytes("/proc/sys/kernel/random/boot_id",64)?,"registered original worker/boot changed");
        for(name,file,kind)in &self.ns{ensure!(storage::id(file)?==storage::id(&storage::namespace(None,name,*kind)?)?,"registered engine namespace changed");}
        let s=self.certificate.metadata()?;ensure!(s.nlink()==0&&s.uid()==0&&s.gid()==0,"registered Root certificate custody changed");Ok(())})();if result.is_err(){self.fenced.store(true,Ordering::Release);}result}
    pub(super) fn from_original_root_retaining(out:&mut Option<Arc<Self>>,fd:&OwnedFd,operations_fd:&OwnedFd,config:Arc<HostIngressConfigOwner>,root:&OwnedFd,root_exe:&OwnedFd,service:&OwnedFd,service_exe:&OwnedFd,
        nonce:&str,constraint:&FinalizationConstraint,worker_tid:u32,fenced:Arc<AtomicBool>)->Result<()> {ensure!(out.is_none(),"registered original owner already staged");
        let raw=super::read_sealed_bytes(fd,MAX)?;let c:Certificate=serde_json::from_slice(&raw).map_err(|_|fault())?;
        ensure!(c.version==1&&c.root_nonce==nonce&&c.constraint==*constraint&&c.ack.root_nonce==nonce&&c.ack.constraint==*constraint&&c.worker_tid==worker_tid
            &&c.ack.kind=="ack"&&c.ack.binding.config_sha256==config.config_sha256()&&c.ack.binding.home_id==config.data().home_id&&c.ack.binding.account_id==config.data().account_id
            &&c.ack.binding.machine_id==config.data().machine_id&&c.ack.binding.store_id==config.data().store_id&&c.ack.binding.incarnation==config.data().incarnation&&c.ack.binding.epoch=="1",
            "registered original Root certificate differs");FinalizationBudget::from_constraint(constraint)?.remaining()?;
        let own=Process::current()?;let image=Image::capture(Path::new(super::ENGINE_PATH))?;ensure!(image.sha256==c.ack.binding.native_sha256,"registered native image differs");
        let root_image=Image::capture(Path::new(super::LAUNCHER_PATH))?;let service_image=Image::capture(Path::new(super::SERVICE_PATH))?;
        ensure!(root_image.sha256==c.ack.binding.launcher_sha256&&service_image.sha256==c.ack.binding.service_sha256,"registered Root/service image differs");
        let worker=OpenOptions::new().read(true).custom_flags(libc::O_CLOEXEC|libc::O_NOFOLLOW).open(format!("/proc/self/task/{worker_tid}/stat"))?;let boot=storage::proc_bytes("/proc/sys/kernel/random/boot_id",64)?;
        ensure!(String::from_utf8(boot.clone())?.trim()==c.ack.boot_id&&read_stat(&worker,worker_tid)?==c.worker_start_ticks,"registered actual boot/worker differs");let ns=namespaces(own.pid)?;
        *out=Some(Arc::new(Self {operations:std::sync::Mutex::new(engine_operations::NativeChannel::capture(operations_fd.try_clone()?)?),data:c.ack,certificate:File::from(fd.try_clone()?),config,own,root:Process::capture(root.try_clone()?)?,service:Process::capture(service.try_clone()?)?,
            image,service_image,service_exe:service_exe.try_clone()?,root_image,root_exe:root_exe.try_clone()?,worker,worker_tid,worker_start:c.worker_start_ticks,ns,boot,fenced}));
        out.as_ref().unwrap().assert_current()?;FinalizationBudget::from_constraint(constraint)?.remaining()?;Ok(())}
}
#[cfg(test)]
#[path="engine_registration_tests.rs"]
mod tests;
