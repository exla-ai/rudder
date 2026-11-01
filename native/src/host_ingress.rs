//! Root-authenticated anonymous host control. DATA never creates custody.
//! Nested under root_storage_bootstrap: constructors are visible only there.
#![cfg(target_os = "linux")]
use anyhow::{ensure, Context, Result};
use crate::root_storage_service::{self as storage, Endpoint, Image, Process};
use rudder_engine::{execution::finalization_budget::{FinalizationBudget, FinalizationConstraint}, journal::ManagedProjectPrepare,
    protocol::canonical_json};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{fs::{self, File, OpenOptions},
    os::{fd::{AsRawFd, OwnedFd}, unix::{fs::{FileExt, MetadataExt, OpenOptionsExt}, process::CommandExt}},
    path::{Component, Path, PathBuf}, sync::{Arc, atomic::{AtomicBool, Ordering}}, time::Duration};

#[path="root_upload.rs"]
pub(crate) mod root_upload;
#[path="host_sidecar_policy.rs"]
mod sidecar_policy;

#[path="host_transport_native.rs"]
mod transport_native;
#[path="host_transport_io.rs"]
mod transport_io;

pub const MAX_HOST_FRAME:usize=32*1024;
pub const MAX_HOST_COMMAND_MS:u32=20_000;
pub const HOST_CONFIG_PATH:&str="/run/rudder-host-control/config.json";
const TOKEN_PATH:&str="/run/rudder-host-control/token";
const NODE:&str="/usr/local/bin/node";
const SIDECAR:&str="/opt/rudder-host-control/host-control.mjs";
const SIDECAR_MANIFEST:&str="/opt/rudder-host-control/manifest.json";
const SIDECAR_POLICY:&str="/usr/local/share/rudder-managed-image/sidecar-policy-pin.json";
const RUNTIME_ROOT:&str="/opt/rudder-managed-runtime/v1";
// Finite image-owned destinations. Protected original config chooses one;
// caller URLs, environment, redirects and runtime hostname overrides do not.
const PRODUCTION_CONTROL_ORIGIN:&str="https://rudder-cloud-control.fly.dev";
const QUALIFICATION_CONTROL_ORIGIN:&str="https://rudder-v4-qualification.fly.dev";
fn cloud_control_origin(value:&str)->Result<&'static str> {match value {
    PRODUCTION_CONTROL_ORIGIN=>Ok(PRODUCTION_CONTROL_ORIGIN),
    QUALIFICATION_CONTROL_ORIGIN=>Ok(QUALIFICATION_CONTROL_ORIGIN),
    _=>Err(anyhow::anyhow!("host Cloud origin is not an image-owned control plane")),
}}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HostReceiptStorePolicy { InitializeEmptyV1, OpenExistingV1 }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum HostRuntimeProfile { FlyManagedImageV1 { user_name:String, user_email:String } }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageRuntimeEntry { pub path:PathBuf, pub sha256:String, pub size_bytes:u64 }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageRuntimeManifest { pub profile_id:String, pub manifest_sha256:String, pub entries:Vec<ImageRuntimeEntry> }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AllowedPreparedProject { pub alias:String, pub checkout:PathBuf, pub home:PathBuf, pub cas:PathBuf,
    pub endpoints:PathBuf, pub prepare:ManagedProjectPrepare }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostBakedRuntimePolicy {pub profile:String,pub inventory:rudder_engine::protocol::ObjectRef}
impl HostBakedRuntimePolicy {
    pub(super) fn validate(&self)->Result<()>{self.inventory.validate()?;ensure!(self.profile=="baked-cas-v1"&&self.inventory.kind==rudder_engine::protocol::ObjectKind::Blob
        &&self.inventory.size_bytes>0&&self.inventory.size_bytes<=16*1024*1024,"runtime baked inventory profile invalid");Ok(())}
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostImagePreseedPolicy {pub image:String,pub image_receipt_sha256:String,pub full_manifest_sha256:String,#[serde(default,skip_serializing_if="Option::is_none")]pub batch_transport:Option<String>,
    #[serde(default,skip_serializing_if="Option::is_none")]pub runtime_mount:Option<HostBakedRuntimePolicy>}
#[derive(Clone,Copy,Debug,PartialEq,Eq,Serialize,Deserialize)]
pub enum HostOwnerMode {#[serde(rename="provider-fenced-v1")]ProviderFencedV1}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostOwnerLeaseSafety {pub qualification_receipt_sha256:String,pub margin_milliseconds:u32,pub engine_rate_error_ppm:u32,pub database_rate_error_ppm:u32,pub max_positive_clock_step_milliseconds:u32}
impl HostOwnerLeaseSafety {
    pub(crate) fn reserve_nanoseconds(&self)->Result<u64> {sha(&self.qualification_receipt_sha256)?;ensure!(self.qualification_receipt_sha256.bytes().any(|b|b!=b'0'),"owner clock qualification absent");
        ensure!(self.engine_rate_error_ppm<1_000_000&&self.database_rate_error_ppm<1_000_000&&self.margin_milliseconds<15_000&&self.max_positive_clock_step_milliseconds<15_000,"owner clock policy invalid");
        let duration=(15_000u64-u64::from(self.max_positive_clock_step_milliseconds))*1_000_000*(1_000_000-u64::from(self.engine_rate_error_ppm))/(1_000_000+u64::from(self.database_rate_error_ppm));
        let effective=duration.checked_sub(u64::from(self.margin_milliseconds)*1_000_000+1).context("owner clock policy duration unavailable")?;ensure!(effective>0,"owner clock duration unavailable");Ok(15_000_000_000-effective)}
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostIngressConfig { pub version:u32, pub machine_id:String, pub store_id:String, pub incarnation:String,
    pub account_id:String, pub home_id:String, pub cloud_origin:String, pub state_directory:PathBuf,
    pub store_policy:HostReceiptStorePolicy, pub projects:Vec<AllowedPreparedProject>,
    pub runtime_profile:HostRuntimeProfile, pub runtime_manifest:ImageRuntimeManifest,
    #[serde(default)] pub image_preseed:Option<HostImagePreseedPolicy>,
    #[serde(default,skip_serializing_if="Option::is_none")] pub runtime_qualification:Option<super::runtime_image::qualification::RuntimeQualificationScope>,
    #[serde(default,skip_serializing_if="Option::is_none")] pub sdk_initial_source:Option<SdkInitialSourceProfile>,
    #[serde(default,skip_serializing_if="Option::is_none")] pub compiler_sdk:Option<rudder_engine::execution::compiler_sdk::CompilerSdkCatalogData>,
    #[serde(default,skip_serializing_if="Option::is_none")] pub compiler_qualification:Option<super::runtime_image::compiler_qualification::CompilerQualificationScope>,
    #[serde(default,skip_serializing_if="Option::is_none")] pub owner_mode:Option<HostOwnerMode>,
    #[serde(default)] pub owner_lease_safety:Option<HostOwnerLeaseSafety>,
    /// Explicit qualified image/profile opt-in only. Absent remains V1.
    /// This policy choice mints no source/task/Root-ready authority.
    #[serde(default,skip_serializing_if="Option::is_none")] pub seed_profile:Option<rudder_vcs_bridge::seed::SeedProfile> }
#[derive(Clone,Copy,Debug,PartialEq,Eq,Serialize,Deserialize)]
pub enum SdkInitialSourceProfile {#[serde(rename="sdk-fixed-suite-v1")] FixedSuiteV1}
#[derive(Clone,Copy,Debug,PartialEq,Eq,Serialize,Deserialize)]
pub enum SdkBuildCase {#[serde(rename="accepted")] Accepted,#[serde(rename="proc-deny")] ProcDenied,#[serde(rename="job-deny")] JobDenied}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all="snake_case")]
pub enum HostBackend { Codex, Claude }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag="op", deny_unknown_fields)]
pub enum HostOperation {
    OpenPreparedProjectV1 { project_alias:String, backend:HostBackend, model:String, profile_id:String },
    StartTaskV1 { project_alias:String, backend:HostBackend, model:String, profile_id:String, text:String },
    BuildCurrentSourceSdkV1 {project_alias:String,case:SdkBuildCase,repeat_identical:bool},
    ReceiptV1 { lookup_command_key:String },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostCommand { pub version:u32, pub session_generation:String, pub rpc_id:String, pub command_key:String,
    pub operation:HostOperation, pub constraint:FinalizationConstraint }
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all="snake_case")]
pub enum HostReplyError { Rejected, Busy, Expired, EffectUnknown, Internal }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostReply { pub version:u32, pub session_generation:String, pub rpc_id:String, pub command_key:String,
    pub result:Option<serde_json::Value>, pub error:Option<HostReplyError> }

fn text(value:&str,max:usize)->Result<()> {ensure!(!value.is_empty() && value.len()<=max && !value.chars().any(char::is_control),"host bounded text invalid");Ok(())}
fn key(value:&str)->Result<()> {text(value,128)?;ensure!(value.bytes().all(|b|b.is_ascii_alphanumeric() || b"-_:.".contains(&b)),"host stable identity invalid");Ok(())}
fn uuid(value:&str)->Result<()> {ensure!(uuid::Uuid::parse_str(value).is_ok_and(|v|v.get_version_num()==4 && v.to_string()==value),"host generation invalid");Ok(())}
fn path(value:&Path)->Result<()> {ensure!(value.is_absolute() && value.as_os_str().len()<=4096
    && value.components().all(|c|matches!(c,Component::RootDir|Component::Normal(_)))
    && value.to_str().is_some_and(|s|s.trim()==s && !s.chars().any(char::is_control)),"host explicit path invalid");Ok(())}
fn sha(value:&str)->Result<()> {ensure!(value.len()==64 && value.bytes().all(|b|b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),"host image digest invalid");Ok(())}
impl HostIngressConfig {
    pub(super) fn assert_unscoped_operations(&self)->Result<()> {ensure!(self.runtime_qualification.is_none()&&self.compiler_qualification.is_none(),"disposable qualification scope refuses host and general engine operations");Ok(())}
    pub(super) fn validate_for_runtime_mount(&self)->Result<()> {self.validate()?;
        if let Some(sdk)=&self.compiler_sdk{sdk.validate_data()?;}
        if let Some(scope)=&self.compiler_qualification{scope.validate_config(self)?;}
        ensure!(self.image_preseed.as_ref().and_then(|p|p.runtime_mount.as_ref()).is_some(),"runtime immutable image profile unconfigured");Ok(())}
    fn validate(&self)->Result<()> {
        ensure!(self.version==1 && !self.projects.is_empty() && self.projects.len()<=8,"host config version/count invalid");
        for value in [&self.machine_id,&self.store_id,&self.incarnation,&self.account_id,&self.home_id] {key(value)?;}
        // No arbitrary URL, credentials, path, redirect or cleartext transport.
        cloud_control_origin(&self.cloud_origin)?;
        if let Some(p)=&self.image_preseed {
            if let Some(runtime)=&p.runtime_mount {runtime.validate()?;}
            ensure!(p.batch_transport.as_deref().is_none_or(|v|v=="batch-v1"),"unsupported image batch transport");sha(&p.image_receipt_sha256)?;sha(&p.full_manifest_sha256)?;
            let Some((name,digest))=p.image.split_once("@sha256:")else{return Err(anyhow::anyhow!("host image preseed image not pinned"));};
            ensure!(name=="registry.fly.io/rudder-workers-exla","host image registry policy invalid");sha(digest)?;}
        if let Some(scope)=&self.runtime_qualification{scope.validate_config(self)?;}
        if let Some(sdk)=&self.compiler_sdk{sdk.validate_data()?;ensure!(self.image_preseed.as_ref().and_then(|p|p.runtime_mount.as_ref()).is_some(),"SDK catalog requires original Root runtime mount");}
        if let Some(scope)=&self.compiler_qualification{scope.validate_config(self)?;}
        if self.sdk_initial_source.is_some(){self.assert_unscoped_operations()?;ensure!(self.compiler_sdk.is_some()&&self.image_preseed.as_ref().and_then(|p|p.runtime_mount.as_ref()).is_some(),"fixed initial SDK source needs actual configured ordinary SDK/runtime profile");}
        ensure!(self.owner_mode.is_none()||self.owner_lease_safety.is_none(),"provider ownership cannot reinterpret a clock lease policy");
        if let Some(safety)=&self.owner_lease_safety{safety.reserve_nanoseconds()?;}
        path(&self.state_directory)?;let mut aliases=std::collections::BTreeSet::new();
        for p in &self.projects {key(&p.alias)?;ensure!(aliases.insert(&p.alias),"host project alias duplicate");
            for value in [&p.checkout,&p.home,&p.cas,&p.endpoints] {path(value)?;}
            p.prepare.validate()?;ensure!(p.prepare.namespace.home==p.home && p.prepare.namespace.repository==p.checkout,
                "host original managed prepare identity differs");}
        let HostRuntimeProfile::FlyManagedImageV1 {user_name,user_email}=&self.runtime_profile;
        text(user_name,256)?;text(user_email,256)?;
        ensure!(self.runtime_manifest.profile_id=="fly-managed-image-v1" && !self.runtime_manifest.entries.is_empty()
            && self.runtime_manifest.entries.len()<=64,"host runtime manifest invalid");sha(&self.runtime_manifest.manifest_sha256)?;
        let mut paths=std::collections::BTreeSet::new();for entry in &self.runtime_manifest.entries {path(&entry.path)?;sha(&entry.sha256)?;
            ensure!(entry.path.starts_with(RUNTIME_ROOT) && paths.insert(&entry.path) && entry.size_bytes>0 && entry.size_bytes<=512*1024*1024,
                "host runtime entry invalid");}
        ensure!(canonical_json(self)?.len()<=MAX_HOST_FRAME,"host config exceeds bound");Ok(())
    }
}
impl HostCommand {
    pub fn validate(&self)->Result<()> {
        ensure!(self.version==1,"host command version invalid");uuid(&self.session_generation)?;key(&self.rpc_id)?;key(&self.command_key)?;
        match &self.operation {
            HostOperation::ReceiptV1 {lookup_command_key}=>key(lookup_command_key)?,
            HostOperation::BuildCurrentSourceSdkV1 {project_alias,..}=>key(project_alias)?,
            HostOperation::OpenPreparedProjectV1 {project_alias,model,profile_id,..} |
            HostOperation::StartTaskV1 {project_alias,model,profile_id,..}=>{key(project_alias)?;text(model,256)?;text(profile_id,256)?;}
        }
        if let HostOperation::StartTaskV1 {text:value,..}=&self.operation {ensure!(!value.trim().is_empty() && value.len()<=24*1024 && !value.contains('\0'),"host task exceeds bound");}
        let c=serde_json::to_value(&self.constraint)?;
        ensure!(c["maximum_milliseconds"].as_u64().is_some_and(|v|(1..=u64::from(MAX_HOST_COMMAND_MS)).contains(&v)),"host command original timeout exceeds policy");
        FinalizationConstraint::from_json(&canonical_json(&self.constraint)?)?;
        ensure!(canonical_json(self)?.len()<=MAX_HOST_FRAME,"host command exceeds bound");Ok(())
    }
}
impl HostReply {
    fn validate_for(&self,command:&HostCommand)->Result<()> {
        ensure!(self.version==1 && self.session_generation==command.session_generation && self.rpc_id==command.rpc_id
            && self.command_key==command.command_key && self.result.as_ref().is_some_and(|v|!v.is_null())!=self.error.is_some(),"host reply binding differs");
        ensure!(canonical_json(self)?.len()<=MAX_HOST_FRAME,"host reply exceeds bound");Ok(())
    }
}

#[derive(Clone,PartialEq,Eq)]
struct Stamp { dev:u64,ino:u64,len:u64,ctime:(i64,i64),mtime:(i64,i64) }
impl Stamp {
    fn of(file:&File,mode:u32)->Result<Self> {let m=file.metadata()?;
        ensure!(m.is_file() && m.uid()==0 && m.gid()==0 && m.nlink()==1 && m.mode()&0o7777==mode,"host provisioned file ownership/mode invalid");
        Ok(Self {dev:m.dev(),ino:m.ino(),len:m.len(),ctime:(m.ctime(),m.ctime_nsec()),mtime:(m.mtime(),m.mtime_nsec())})}
    fn check(&self,file:&File,mode:u32)->Result<()> {ensure!(*self==Self::of(file,mode)?,"host retained file changed");Ok(())}
}
fn read_at(file:&File,limit:usize)->Result<Vec<u8>> {let len=usize::try_from(file.metadata()?.len())?;ensure!(len>0 && len<=limit,"host file exceeds bound");
    let mut bytes=vec![0;len];ensure!(file.read_at(&mut bytes,0)?==len,"host original file read incomplete");Ok(bytes)}
fn open_root(path:&Path,mode:u32)->Result<(File,Stamp)> {
    ensure!(path.canonicalize()?==path,"host image/config path redirects");
    for parent in path.parent().context("host file parent absent")?.ancestors() {let m=fs::symlink_metadata(parent)?;
        ensure!(m.is_dir() && !m.file_type().is_symlink() && m.uid()==0 && m.gid()==0 && m.mode()&0o022==0,"host file parent not root controlled");}
    let file=OpenOptions::new().read(true).custom_flags(libc::O_CLOEXEC|libc::O_NOFOLLOW|libc::O_NONBLOCK).open(path)?;
    let stamp=Stamp::of(&file,mode)?;Ok((file,stamp))
}
struct SidecarWitness { process:Process, image:Image, exe:OwnedFd, namespaces:Vec<(String,File,i32)> }
impl SidecarWitness {
    fn check_transferred(&self)->Result<()> {self.process.live()?;storage::floor(self.process.pid,1500)?;
        self.image.check()?;self.image.matches_fd(&self.exe)?;
        for (name,file,kind) in &self.namespaces {ensure!(storage::id(file)?==storage::id(&storage::namespace(None,name,*kind)?)?,"host sidecar namespace differs");}Ok(())}
}
/// Immutable original config FD and actual bootstrap/process witnesses. No
/// public constructor, Clone, Serde, Debug, nonce/FD accessor or path adoption.
/// Root retains the original sidecar Child and checks its actual proc-exe before
/// EVERY forwarded command. Native never receives a token or its descriptor.
pub struct HostIngressConfigOwner { file:File, stamp:Stamp, digest:String, data:HostIngressConfig, bootstrap_nonce:String,
    root:Process, own:Process, namespaces:Vec<(String,File,i32)>, boot:Vec<u8>, fenced:AtomicBool }
impl HostIngressConfigOwner {
    pub fn data(&self)->&HostIngressConfig {&self.data}
    pub fn config_sha256(&self)->&str {&self.digest}
    pub fn assert_current(&self)->Result<()> {let result=(||->Result<()> {
        ensure!(!self.fenced.load(Ordering::Acquire),"host config owner fenced");self.stamp.check(&self.file,0o600)?;
        self.root.live()?;storage::floor(self.root.pid,0)?;self.own.live()?;ensure!(self.own.pid==std::process::id(),"host config moved to another process");uuid(&self.bootstrap_nonce)?;
        for (name,file,kind) in &self.namespaces {ensure!(storage::id(file)?==storage::id(&storage::namespace(None,name,*kind)?)?,"host config namespace changed");}
        ensure!(self.boot==storage::proc_bytes("/proc/sys/kernel/random/boot_id",64)?,"host original boot changed");Ok(())})();
        if result.is_err() {self.fenced.store(true,Ordering::Release);}result}
}
/// Actual Root-authenticated endpoint. All methods are OFF App; close/error is
/// uncertainty, never a joined sidecar, accepted task or permission to replay.
pub struct HostIngress { endpoint:Endpoint, config:Arc<HostIngressConfigOwner>, session_generation:String, sidecar:SidecarWitness,
    command:Option<HostCommand>, received:Option<storage::Packet>, reply:Option<Vec<u8>>, reply_sent:bool, transport:transport_native::NativeTransport, expired_reply:Option<HostReply>, transport_errors:Vec<anyhow::Error>, fenced:AtomicBool }
impl HostIngress {
    pub fn config(&self)->&Arc<HostIngressConfigOwner> {&self.config}
    pub fn assert_current(&self)->Result<()> {let result=(||->Result<()> {
        ensure!(!self.fenced.load(Ordering::Acquire),"host ingress fenced");self.config.assert_current()?;Ok(())})();
        if result.is_err() {self.fenced.store(true,Ordering::Release);}result}
    pub fn try_receive(&mut self)->Result<Option<HostCommand>> {
        let result=(||->Result<Option<HostCommand>> {self.assert_current()?;
            self.transport.flush_acceptance(&self.endpoint,&self.config)?;
            if self.expired_reply.is_some(){if !self.flush_expired_retaining()?{return Ok(None);}}
            ensure!(self.command.is_none(),"host original command awaits retained settlement");
            let Some(packet)=self.endpoint.receive()? else {return Ok(None);};
            if transport_native::NativeTransport::is_control(&packet){self.transport.consume_retaining(packet,&self.config,&self.endpoint)?;self.session_generation=self.transport.generation().to_owned();return Ok(None);}self.received=Some(packet);
            let packet=self.received.as_ref().unwrap();
            ensure!(packet.credentials.pid==self.config.root.pid as i32 && packet.credentials.uid==0 && packet.credentials.gid==0
                && packet.fds.is_empty() && packet.bytes.len()<=MAX_HOST_FRAME,"host command is not actual Root forwarded");
            let command:HostCommand=serde_json::from_slice(&packet.bytes).map_err(|_|anyhow::anyhow!("host command encoding invalid"))?;
            // Stage original DATA BEFORE late/binding/time checks. Errors never
            // reopen this slot or permit a second dispatch/reconstruction.
            self.command=Some(command);let command=self.command.as_ref().unwrap();command.validate()?;
            self.config.data().assert_unscoped_operations()?;
            ensure!(command.session_generation==self.session_generation&&self.transport.online(),"host command has no genuine current transport generation");
            if FinalizationBudget::from_constraint(&command.constraint).and_then(|b|b.remaining()).is_err(){self.expired_reply=Some(HostReply {version:1,session_generation:command.session_generation.clone(),rpc_id:command.rpc_id.clone(),command_key:command.command_key.clone(),result:None,error:Some(HostReplyError::Expired)});self.flush_expired_retaining()?;return Ok(None);}
            self.assert_current()?;let command=command.clone();self.received.take();Ok(Some(command))})();
        if result.is_err() {self.fenced.store(true,Ordering::Release);}result
    }
    /// false means EAGAIN/no packet dispatch. A known send is recorded BEFORE
    /// postchecks; an error never retries a sent/possibly-sent reply.
    pub fn try_reply(&mut self,reply:&HostReply)->Result<bool> {
        let result=(||->Result<bool> {self.assert_current()?;ensure!(!self.reply_sent,"host original reply already dispatched");
            let command=self.command.as_ref().context("host original command absent")?;reply.validate_for(command)?;
            // Reply DATA may settle an original command after a socket/clock loss.
            // This never admits a fresh command, source effect, or lease.
            let bytes=canonical_json(reply)?;if let Some(original)=&self.reply {ensure!(*original==bytes,"host reply changed while pending");}
            else {self.reply=Some(bytes);}
            if !self.endpoint.send(self.reply.as_ref().unwrap(),None)? {return Ok(false);}
            self.reply_sent=true;self.assert_current()?;
            self.command.take();self.reply.take();self.reply_sent=false;Ok(true)})();
        if result.is_err() {self.fenced.store(true,Ordering::Release);}result
    }
    fn flush_expired_retaining(&mut self)->Result<bool>{let Some(reply)=&self.expired_reply else{return Ok(true);};self.assert_current()?;if !self.endpoint.send(&canonical_json(reply)?,None)?{return Ok(false);}self.expired_reply.take();self.command.take();self.received.take();Ok(true)}
    // Only the authenticated original Root bootstrap can supply these actual
    // descriptors. A manifest or saved JSON cannot call this factory.
    pub(super) fn from_root_transfer(config:OwnedFd,endpoint:OwnedFd,root:OwnedFd,sidecar:OwnedFd,exe:OwnedFd,
        namespace_fds:Vec<(String,OwnedFd,i32)>,nonce:String,generation:String)->Result<Self> {
        uuid(&nonce)?;uuid(&generation)?;let config_flags=unsafe {libc::fcntl(config.as_raw_fd(),libc::F_GETFL)};
        ensure!(config_flags>=0 && config_flags&libc::O_ACCMODE==libc::O_RDONLY,"host config is not original readonly descriptor");
        let file=File::from(config);let stamp=Stamp::of(&file,0o600)?;
        let bytes=read_at(&file,MAX_HOST_FRAME)?;let data:HostIngressConfig=serde_json::from_slice(&bytes)?;data.validate()?;
        let owner=Arc::new(HostIngressConfigOwner {file,stamp,digest:format!("{:x}",Sha256::digest(&bytes)),data,bootstrap_nonce:nonce,
            root:Process::capture(root)?,own:Process::current()?,namespaces:vec![
                ("mnt".into(),storage::namespace(None,"mnt",libc::CLONE_NEWNS)?,libc::CLONE_NEWNS),
                ("user".into(),storage::namespace(None,"user",libc::CLONE_NEWUSER)?,libc::CLONE_NEWUSER)],
            boot:storage::proc_bytes("/proc/sys/kernel/random/boot_id",64)?,fenced:AtomicBool::new(false)});
        // Config policy outlives the socket. Accepted work retains this Arc
        // without depending on sidecar liveness or a transport generation.
        let sidecar=SidecarWitness {process:Process::capture(sidecar)?,image:Image::capture(Path::new(NODE))?,exe,
            namespaces:namespace_fds.into_iter().map(|(n,f,k)|(n,File::from(f),k)).collect()};
        ensure!(sidecar.namespaces.len()==4,"host original namespace witnesses incomplete");owner.assert_current()?;sidecar.check_transferred()?;
        Ok(Self {endpoint:Endpoint::capture(endpoint)?,config:owner,transport:transport_native::NativeTransport::initial(generation.clone()),session_generation:generation,sidecar,command:None,received:None,reply:None,reply_sent:false,expired_reply:None,transport_errors:Vec::new(),fenced:AtomicBool::new(false)})
    }
}

#[derive(Clone,Debug,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct SidecarFile { path:PathBuf, sha256:String, size_bytes:u64 }
#[derive(Clone,Debug,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct SidecarManifest { version:u32, files:Vec<SidecarFile> }
struct ImmutableFile { file:File, stamp:Stamp, path:PathBuf, mode:u32 }
impl ImmutableFile {fn check(&self)->Result<()> {self.stamp.check(&self.file,self.mode)?;
    for parent in self.path.parent().context("host image parent absent")?.ancestors() {let m=fs::symlink_metadata(parent)?;
        ensure!(m.is_dir() && !m.file_type().is_symlink() && m.uid()==0 && m.gid()==0 && m.mode()&0o022==0,"host image ancestor changed");}
    let m=fs::symlink_metadata(&self.path)?;
    ensure!(!m.file_type().is_symlink() && (m.dev(),m.ino())==(self.stamp.dev,self.stamp.ino),"host image path replaced");Ok(())}}
fn complete_sidecar_files()->Result<std::collections::BTreeSet<PathBuf>> {
    let root=Path::new("/opt/rudder-host-control");let mut todo=vec![root.to_path_buf()];let mut dirs=0usize;
    let mut files=std::collections::BTreeSet::new();let mut entries=0usize;
    while let Some(directory)=todo.pop() {dirs+=1;ensure!(dirs<=64,"host sidecar directory count exceeds bound");
        let d=fs::symlink_metadata(&directory)?;ensure!(d.is_dir() && !d.file_type().is_symlink() && d.uid()==0 && d.gid()==0
            && d.mode()&0o022==0,"host sidecar directory not immutable");
        for entry in fs::read_dir(directory)? {let entry=entry?;entries+=1;ensure!(entries<=128,"host sidecar entry count exceeds bound");
            let path=entry.path();let metadata=fs::symlink_metadata(&path)?;
            ensure!(!metadata.file_type().is_symlink(),"host sidecar closure has symlink");
            if metadata.is_dir() {todo.push(path);}else {
                ensure!(metadata.is_file(),"host sidecar closure has special file");
                let relative=path.strip_prefix(root)?.to_path_buf();if relative!=Path::new("manifest.json") {files.insert(relative);}
                ensure!(files.len()<=64,"host sidecar file count exceeds bound");
            }
        }
    }Ok(files)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Ready {kind:String,version:u32,session_generation:String,nonce:String,home_id:String,incarnation:String,config_sha256:String}
struct RootPending {command:HostCommand,bytes:Vec<u8>,forwarded:bool,reply:Option<storage::Packet>,sidecar_instance:String}
/// Root-owned original effects; no Drop teardown/retry or external activation.
pub(super) struct RootHostControl { transport:transport_io::TransportIo, volume_init:Option<volume_init::HostVolumeInit>,original:FinalizationBudget,nonce:String,file:File,stamp:Stamp,data:HostIngressConfig,digest:String,
    token:File,token_stamp:Stamp,images:Vec<ImmutableFile>,node:Image,sidecar_pair:Endpoint,child_endpoint:OwnedFd,
    native_pair:Endpoint,native_endpoint:OwnedFd,uptime:File,boot:Vec<u8>,child:Option<super::ChildOwner>,
    attempted:bool,generation:Option<String>,namespaces:Vec<(String,File,i32)>,exe:Option<OwnedFd>,
    runtime_qualification_inbound:Option<storage::Packet>,
    runtime_received:Option<storage::Packet>,operation_inbound:Option<storage::Packet>,ready_packet:Option<storage::Packet>,pending:Option<RootPending>,fenced:bool,fence_sent:bool,fence_unknown:bool }
impl RootHostControl {
    pub(super) fn capture_optional(original:&FinalizationBudget,nonce:&str)->Result<Option<Self>> {
        original.remaining()?;
        let present=|p:&str|->Result<bool> {match fs::symlink_metadata(p) {Ok(_)=>Ok(true),Err(e) if e.kind()==std::io::ErrorKind::NotFound=>Ok(false),Err(e)=>Err(e.into())}};
        let config=present(HOST_CONFIG_PATH)?;let token=present(TOKEN_PATH)?;if !config && !token {return Ok(None);}
        ensure!(config && token,"host optional config/token pair incomplete");
        let directory=fs::symlink_metadata("/run/rudder-host-control")?;
        ensure!(directory.is_dir() && !directory.file_type().is_symlink() && directory.uid()==0 && directory.gid()==0
            && directory.mode()&0o7777==0o700,"host provisioning directory invalid");
        let (file,stamp)=open_root(Path::new(HOST_CONFIG_PATH),0o600)?;let bytes=read_at(&file,MAX_HOST_FRAME)?;
        let data:HostIngressConfig=serde_json::from_slice(&bytes).map_err(|_|anyhow::anyhow!("host provisioned config invalid"))?;data.validate()?;
        let (token,token_stamp)=open_root(Path::new(TOKEN_PATH),0o600)?;ensure!((32..=4096).contains(&token_stamp.len),"host token file bound invalid");
        let (manifest,manifest_stamp)=open_root(Path::new(SIDECAR_MANIFEST),0o444)?;
        let manifest_bytes=read_at(&manifest,32*1024)?;let manifest_data:SidecarManifest=serde_json::from_slice(&manifest_bytes)?;
        sidecar_policy::validate_manifest(&manifest_data)?;
        let (policy,policy_stamp)=open_root(Path::new(SIDECAR_POLICY),0o444)?;
        sidecar_policy::validate_pin(&read_at(&policy,64*1024)?,&manifest_bytes,&manifest_data)?;
        let mut images=vec![ImmutableFile {file:manifest,stamp:manifest_stamp,path:SIDECAR_MANIFEST.into(),mode:0o444},
            ImmutableFile {file:policy,stamp:policy_stamp,path:SIDECAR_POLICY.into(),mode:0o444}];
        let mut names=std::collections::BTreeSet::new();let mut total=0u64;
        for entry in manifest_data.files {sha(&entry.sha256)?;ensure!(entry.path.is_relative()
            && entry.path.components().all(|p|matches!(p,Component::Normal(_))) && names.insert(entry.path.clone()),"host sidecar manifest path invalid");
            total=total.checked_add(entry.size_bytes).context("host image bytes overflow")?;ensure!(total<=8*1024*1024,"host sidecar dependencies exceed bound");
            let path=Path::new("/opt/rudder-host-control").join(&entry.path);let (file,stamp)=open_root(&path,0o555)?;
            ensure!(stamp.len==entry.size_bytes && format!("{:x}",Sha256::digest(read_at(&file,8*1024*1024)?))==entry.sha256,"host sidecar dependency differs");
            images.push(ImmutableFile {file,stamp,path,mode:0o555});}
        ensure!(names==complete_sidecar_files()?,"host sidecar manifest omits/adds dependency files");
        ensure!(names.contains(Path::new("host-control.mjs")) && names.contains(Path::new("node_modules/ws/wrapper.mjs")),"host sidecar closure incomplete");
        let (sidecar_pair,child_endpoint)=storage::bootstrap_socket_pair()?;let (native_pair,native_endpoint)=storage::bootstrap_socket_pair()?;
        let uptime=OpenOptions::new().read(true).custom_flags(libc::O_CLOEXEC|libc::O_NOFOLLOW|libc::O_NONBLOCK).open("/proc/uptime")?;
        let mut stat:libc::statfs=unsafe {std::mem::zeroed()};ensure!(unsafe {libc::fstatfs(uptime.as_raw_fd(),&mut stat)}==0 && stat.f_type==0x9fa0,"host uptime is not actual procfs");
        let value=Self {transport:transport_io::TransportIo::new(sidecar_pair.try_clone()?),volume_init:None,original:original.clone(),nonce:nonce.into(),file,stamp,data,digest:format!("{:x}",Sha256::digest(&bytes)),token,token_stamp,images,
            node:Image::capture(Path::new(NODE))?,sidecar_pair:Endpoint::capture(sidecar_pair)?,child_endpoint,native_pair:Endpoint::capture(native_pair)?,native_endpoint,
            uptime,boot:storage::proc_bytes("/proc/sys/kernel/random/boot_id",64)?,child:None,attempted:false,generation:None,namespaces:Vec::new(),exe:None,runtime_qualification_inbound:None,runtime_received:None,operation_inbound:None,ready_packet:None,pending:None,fenced:false,fence_sent:false,fence_unknown:false};
        value.current()?;original.remaining()?;Ok(Some(value))
    }
    pub(super) fn check_transport_policy_current(&self)->Result<()> {
        ensure!(!self.fenced && self.boot==storage::proc_bytes("/proc/sys/kernel/random/boot_id",64)?,"host original owner fenced/boot changed");
        self.stamp.check(&self.file,0o600)?;self.token_stamp.check(&self.token,0o600)?;self.node.check()?;
        for (path,stamp) in [(HOST_CONFIG_PATH,&self.stamp),(TOKEN_PATH,&self.token_stamp)] {
            let m=fs::symlink_metadata(path)?;ensure!(!m.file_type().is_symlink() && (m.dev(),m.ino())==(stamp.dev,stamp.ino),"host provisioned path replaced");}
        let directory=fs::symlink_metadata("/run/rudder-host-control")?;
        ensure!(directory.is_dir() && directory.uid()==0 && directory.gid()==0 && directory.mode()&0o7777==0o700,"host provisioned parent changed");
        for image in &self.images {image.check()?;}
        if let Some(volume)=&self.volume_init {volume.check_current()?;}
        Ok(())
    }
    pub(super) fn current(&self)->Result<()> {self.check_transport_policy_current()?;
        if let Some(child)=&self.child {let process=child.original()?;process.live()?;self.node.matches_process(process)?;storage::floor(process.pid,1500)?;
            for (name,file,kind) in &self.namespaces {ensure!(storage::id(file)?==storage::id(&storage::namespace(Some(process.pid),name,*kind)?)?
                && storage::id(file)?==storage::id(&storage::namespace(None,name,*kind)?)?,"host original sidecar namespace changed");}}
        Ok(())
    }
    pub(super) fn start_retaining(&mut self,data_fd:&OwnedFd)->Result<()> {
        self.original.remaining()?;self.current()?;ensure!(!self.attempted,"host sidecar spawn cannot replay");self.attempted=true;
        let config:OwnedFd=self.file.try_clone()?.into();let token:OwnedFd=self.token.try_clone()?.into();let uptime:OwnedFd=self.uptime.try_clone()?.into();
        let (mut command,held)=super::mapped_command(NODE,&["--no-addons","--disable-proto=throw",SIDECAR],false,
            &[(&self.child_endpoint,3),(&config,4),(&token,5),(&uptime,6),(data_fd,7)])?;
        unsafe {command.pre_exec(|| {
            // No allocation/user callback after fork. Drop bounding while root.
            if libc::setgroups(0,std::ptr::null())!=0 {return Err(std::io::Error::last_os_error());}
            for cap in 0..=63 {if libc::prctl(libc::PR_CAPBSET_READ,cap,0,0,0)==1 && libc::prctl(libc::PR_CAPBSET_DROP,cap,0,0,0)!=0 {return Err(std::io::Error::last_os_error());}}
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS,1,0,0,0)!=0 || libc::prctl(libc::PR_SET_KEEPCAPS,0,0,0,0)!=0
                || libc::prctl(libc::PR_CAP_AMBIENT,libc::PR_CAP_AMBIENT_CLEAR_ALL,0,0,0)!=0
                || libc::setresgid(1500,1500,1500)!=0 || libc::setresuid(1500,1500,1500)!=0 {return Err(std::io::Error::last_os_error());}
            super::clear_capabilities()?;Ok(())
        });}
        self.original.remaining()?;
        ensure!(matches!(super::spawn_retaining(&mut self.child,&mut command)?,super::SpawnEffect::ChildStored),"host original sidecar spawn failed");drop(held);
        let process=self.child.as_ref().unwrap().original()?;
        self.exe=Some(File::open(format!("/proc/{}/exe",process.pid))?.into());self.node.matches_fd(self.exe.as_ref().unwrap())?;
        for (name,kind) in [("mnt",libc::CLONE_NEWNS),("user",libc::CLONE_NEWUSER),("net",libc::CLONE_NEWNET),("pid",libc::CLONE_NEWPID)] {
            self.namespaces.push((name.into(),storage::namespace(Some(process.pid),name,kind)?,kind));}
        loop {self.original.remaining()?;self.current()?;
            if let Some(packet)=self.sidecar_pair.receive()? {self.ready_packet=Some(packet);let packet=self.ready_packet.as_ref().unwrap();self.actual_sidecar_packet(packet)?;
                let ready:Ready=serde_json::from_slice(&packet.bytes)?;
                ensure!(ready.kind=="ready" && ready.version==1 && ready.nonce==self.nonce && ready.home_id==self.data.home_id
                    && ready.incarnation==self.data.incarnation && ready.config_sha256==self.digest,"host actual WebSocket readiness binding differs");
                uuid(&ready.session_generation)?;self.generation=Some(ready.session_generation);self.original.remaining()?;self.current()?;return Ok(());}
            std::thread::sleep(self.original.phase_limit(Duration::from_millis(2))?);}
    }
    fn actual_sidecar_packet(&self,packet:&storage::Packet)->Result<()> {let process=self.child.as_ref().context("host original sidecar absent")?.original()?;
        ensure!(packet.credentials.pid==process.pid as i32 && packet.credentials.uid==1500 && packet.credentials.gid==1500
            && packet.fds.is_empty() && packet.bytes.len()<=MAX_HOST_FRAME,"host packet is not actual sidecar");Ok(())}
    pub(super) fn transfer_fds(&self)->Result<Vec<(&'static str,OwnedFd)>> {
        self.current()?;let child=self.child.as_ref().context("host sidecar absent")?.original()?;
        let mut fds=vec![("config",self.file.try_clone()?.into()),("endpoint",self.native_endpoint.try_clone()?),
            ("sidecar",child.fd().try_clone()?), ("exe",self.exe.as_ref().context("host sidecar exe absent")?.try_clone()?)];
        for (name,file,_) in &self.namespaces {let label=match name.as_str() {"mnt"=>"mnt","user"=>"user","net"=>"net","pid"=>"pid",_=>anyhow::bail!("host namespace invalid")};
            fds.push((label,file.try_clone()?.into()));}Ok(fds)
    }
    /// Public config only. Root token and registration key NEVER cross this
    /// private service/native channel or a helper's runtime FD capsule.
    pub(super) fn duplicate_runtime_public_config(&self)->Result<OwnedFd>{self.current()?;Ok(self.file.try_clone()?.into())}
    pub(super) fn registration_config(&self)->&HostIngressConfig {&self.data}
    pub(super) fn registration_config_sha(&self)->&str {&self.digest}
    pub(super) fn registration_node_sha(&self)->&str {&self.node.sha256}
    pub(super) fn registration_send(&mut self,bytes:&[u8])->Result<bool> {self.check_transport_policy_current()?;ensure!(bytes.len()<=MAX_HOST_FRAME,"registration relay exceeds bound");if serde_json::from_slice::<serde_json::Value>(bytes)?.get("kind").and_then(|v|v.as_str()).is_some_and(|kind|matches!(kind,"engine_operation_v1"|"runtime_image_qualification_ready_v1"))&&self.transport.offline(){return Ok(false);}self.current()?;self.sidecar_pair.send(bytes,None)}
    pub(super) fn registration_receive_retaining(&mut self,out:&mut Option<storage::Packet>)->Result<()> {
        self.current()?;ensure!(out.is_none(),"registration received packet already retained");
        if let Some(packet)=self.sidecar_pair.receive()? {*out=Some(packet);self.actual_sidecar_packet(out.as_ref().unwrap())?;self.current()?;}Ok(())
    }
    // Explicit inspect-only late ACK read from the ORIGINAL endpoint after a
    // peer exit/fence. It authenticates original SCM PID and never grants
    // liveness, retries an effect or reconstructs a replacement sidecar.
    pub(super) fn registration_receive_settled_retaining(&mut self,out:&mut Option<storage::Packet>)->Result<()> {
        ensure!(out.is_none(),"registration received packet already retained");
        if let Some(packet)=self.sidecar_pair.receive()? {*out=Some(packet);self.actual_sidecar_packet(out.as_ref().unwrap())?;}Ok(())
    }
    pub(in crate::root_storage_bootstrap) fn runtime_qualification_receive_retaining(&mut self,out:&mut Option<storage::Packet>)->Result<()> {
        ensure!(out.is_none(),"original qualification ACK output occupied");
        self.pump_sidecar_transport_retaining()?;self.current()?;
        ensure!(!self.transport.offline(),"qualification transport unknown/offline; no ACK promotion");
        *out=self.runtime_qualification_inbound.take();Ok(())
    }
    fn pump_runtime_retaining(&mut self,_passive:bool)->Result<()> {self.pump_sidecar_transport_retaining()}
    pub(super) fn operation_receive_retaining(&mut self,out:&mut Option<storage::Packet>)->Result<()> {
        ensure!(out.is_none(),"original engine ACK output occupied");if self.operation_inbound.is_none(){self.pump_runtime_retaining(false)?;}*out=self.operation_inbound.take();Ok(())
    }
    pub(super) fn operation_receive_settled_retaining(&mut self,out:&mut Option<storage::Packet>)->Result<()> {
        ensure!(out.is_none(),"original engine ACK output occupied");if self.operation_inbound.is_none(){self.pump_runtime_retaining(true)?;}*out=self.operation_inbound.take();Ok(())
    }
    pub(super) fn generation(&self)->Result<&str> {self.generation.as_deref().context("host actual WebSocket generation absent")}
    pub(super) fn poll_retaining(&mut self,engine:&Process)->Result<()> {
        if self.fenced {self.try_fence_notice();return Ok(());}
        let result=(||->Result<()> {self.check_transport_policy_current()?;engine.live()?;storage::floor(engine.pid,1500)?;
            self.poll_transport_health_retaining(engine)?;
            if let Some(pending)=&self.pending {if FinalizationBudget::from_constraint(&pending.command.constraint).and_then(|b|b.remaining()).is_err(){ensure!(self.transport.orphan_capacity(),"unknown original commands retained at bound");self.transport.retain_orphan(self.pending.take().unwrap());}}
            if let Some(pending)=&mut self.pending {
                self.data.assert_unscoped_operations()?;
                if !pending.forwarded&&!self.transport.offline(){if !self.native_pair.send(&pending.bytes,None)?{return Ok(());}pending.forwarded=true;}
                if let Some(packet)=&pending.reply {if !self.transport.offline()&&Some(pending.command.session_generation.as_str())==self.generation.as_deref(){if self.sidecar_pair.send(&packet.bytes,None)?{self.pending.take();}}}
            }
            self.deliver_late_reply_data_retaining()?;self.check_transport_policy_current()?;engine.live()?;Ok(())})();
        if result.is_err(){self.fenced=true;self.try_fence_notice();}result
    }
    fn try_fence_notice(&mut self) {
        if !self.fence_sent && !self.fence_unknown {
            // This static non-command is authenticated by the same actual Root
            // credentials; native fails its command parser and fences ingress.
            match self.native_pair.send(b"{\"channel_fenced\":true}",None) {
                Ok(true)=>self.fence_sent=true,Ok(false)=>{},Err(_)=>self.fence_unknown=true,
            }
        }
    }
}

#[cfg(test)]
#[path="host_ingress_tests.rs"]
mod tests;

#[path="host_volume_init.rs"]
mod volume_init;

#[cfg(test)]#[path="managed_sdk_host_command_tests.rs"]mod sdk_source_command_tests;
