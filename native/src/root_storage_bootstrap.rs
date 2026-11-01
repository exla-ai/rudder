//! Image-owned machine bootstrap only. No scheduler, profiles, user argv/env,
//! saved-path adoption, replacement engine or implicit service retirement.
#![cfg(target_os = "linux")]
use anyhow::{ensure, Context, Result};
use crate::{root_storage_client::RootStorageClient, root_storage_service::{self as storage, Endpoint, Image, Process}};
use rudder_engine::{execution::finalization_budget::{FinalizationBudget, FinalizationConstraint}, protocol::canonical_json};
use serde::{Deserialize, Serialize};
use std::{any::Any, ffi::CString, fmt, fs::{self, File, OpenOptions}, io::{self, Write},
    os::{fd::{AsRawFd, FromRawFd, OwnedFd}, unix::{fs::{FileExt, MetadataExt, OpenOptionsExt}, process::CommandExt}},
    path::Path, process::{Child, Command, ExitStatus, Stdio}, sync::atomic::{AtomicBool, Ordering}, time::Duration};

pub const ENGINE_PATH: &str = "/opt/rudder/dist/native/rudder-native";
pub const SERVICE_PATH: &str = "/usr/local/bin/rudder-root-storage";
pub const LAUNCHER_PATH: &str = "/usr/local/bin/rudder-machine-root";
const SEALS: i32 = libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;
const DATA_LIMIT: usize = 4096;
const ENGINE_BOOTSTRAP_CAPS:u64=(1<<6)|(1<<7)|(1<<8); // SETGID, SETUID, SETPCAP only.
const DUMPABLE_POLICY:&str="/proc/sys/fs/suid_dumpable";

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BootstrapData { version: u32, nonce: String, constraint: FinalizationConstraint }
impl BootstrapData {
    fn validate(&self) -> Result<()> {
        let id=uuid::Uuid::parse_str(&self.nonce)?;
        ensure!(self.version==1 && id.get_version_num()==4 && id.to_string()==self.nonce,"machine bootstrap data invalid");
        FinalizationBudget::from_constraint(&self.constraint)?.remaining()?; Ok(())
    }
}
fn sealed_data(data: &BootstrapData) -> Result<OwnedFd> {
    let bytes=canonical_json(data)?;ensure!(bytes.len()<=DATA_LIMIT,"machine bootstrap data exceeds bound");
    let name=CString::new("rudder-machine-bootstrap")?;
    let raw=unsafe {libc::syscall(libc::SYS_memfd_create,name.as_ptr(),libc::MFD_CLOEXEC|libc::MFD_ALLOW_SEALING)} as i32;
    ensure!(raw>=0,"machine bootstrap sealed data unavailable");
    let mut file=File::from(unsafe {OwnedFd::from_raw_fd(raw)});file.write_all(&bytes)?;
    ensure!(unsafe {libc::fcntl(file.as_raw_fd(),libc::F_ADD_SEALS,SEALS)}==0,"machine bootstrap data seal failed");
    let read:OwnedFd=OpenOptions::new().read(true).custom_flags(libc::O_CLOEXEC).open(format!("/proc/self/fd/{}",file.as_raw_fd()))?.into();
    let original=file.metadata()?;let actual=fs::metadata(format!("/proc/self/fd/{}",read.as_raw_fd()))?;
    ensure!((original.dev(),original.ino())==(actual.dev(),actual.ino()),"machine bootstrap sealed file changed");Ok(read)
}
fn read_data(fd:&OwnedFd)->Result<BootstrapData> {
    let flags=unsafe {libc::fcntl(fd.as_raw_fd(),libc::F_GETFL)};
    let seals=unsafe {libc::fcntl(fd.as_raw_fd(),libc::F_GET_SEALS)};
    let file=File::from(fd.try_clone()?);let m=file.metadata()?;
    ensure!(flags>=0 && flags & libc::O_ACCMODE==libc::O_RDONLY && seals>=0 && seals & SEALS==SEALS
        && m.is_file() && m.nlink()==0 && m.uid()==0 && m.gid()==0 && m.len()>0 && m.len()<=DATA_LIMIT as u64,
        "machine bootstrap requires original readonly sealed root data");
    let mut bytes=vec![0u8;m.len() as usize];let n=file.read_at(&mut bytes,0)?;
    ensure!(n==bytes.len(),"machine bootstrap data read uncertain");let data:BootstrapData=serde_json::from_slice(&bytes)?;data.validate()?;Ok(data)
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all="snake_case")]
enum Message { EngineWaiting, ServicePidfd, ServiceImage, ClientAuthenticated, ParentAccepted, HostServing(u32), Quarantine }
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Frame { version:u32, serial:u64, nonce:String, constraint:FinalizationConstraint, message:Message }
struct Gate {
    endpoint:Endpoint,peer:Process,uid:u32,data:BootstrapData,budget:FinalizationBudget,sent:u64,received:u64,
    pending_packet:Option<storage::Packet>,host_ack:Option<u32>,host_ack_fd:Option<OwnedFd>,
}
impl Gate {
    fn capture(endpoint:OwnedFd,peer:OwnedFd,uid:u32,data:BootstrapData,budget:FinalizationBudget)->Result<Self> {
        ensure!(budget.constraint()==data.constraint,"machine gate original budget differs");budget.remaining()?;
        Ok(Self {endpoint:Endpoint::capture(endpoint)?,peer:Process::capture(peer)?,uid,data,budget,sent:0,received:0,
            pending_packet:None,host_ack:None,host_ack_fd:None})
    }
    fn frame(&mut self,message:Message)->Result<Vec<u8>> {
        self.sent=self.sent.checked_add(1).context("machine bootstrap serial exhausted")?;
        storage::encode(&Frame {version:1,serial:self.sent,nonce:self.data.nonce.clone(),constraint:self.data.constraint.clone(),message},&[])
    }
    fn send_startup(&mut self,message:Message,fd:Option<&OwnedFd>)->Result<()> {
        let bytes=self.frame(message)?;
        loop {self.peer.live()?;self.budget.remaining()?;
            if self.endpoint.send(&bytes,fd)? {self.budget.remaining()?;return Ok(());}
            std::thread::sleep(Duration::from_millis(2));}
    }
    fn try_quarantine(&mut self) {if let Ok(bytes)=self.frame(Message::Quarantine) {let _=self.endpoint.send(&bytes,None);}}
    fn poll(&mut self)->Result<Option<(Message,Option<OwnedFd>)>> {
        ensure!(self.pending_packet.is_none(),"machine uncertain received bootstrap packet retained");self.peer.live()?;
        let Some(packet)=self.endpoint.receive()? else {return Ok(None);};self.pending_packet=Some(packet);
        let packet=self.pending_packet.as_ref().unwrap();
        ensure!(packet.credentials.pid==self.peer.pid as i32 && packet.credentials.uid==self.uid && packet.credentials.gid==self.uid,
            "machine bootstrap original peer credentials differ");
        let (frame,payload):(Frame,_)=storage::decode(&packet.bytes)?;
        ensure!(frame.version==1 && frame.serial==self.received.checked_add(1).context("machine bootstrap receive serial exhausted")?
            && frame.nonce==self.data.nonce && frame.constraint==self.data.constraint && payload.is_empty(),"machine bootstrap frame binding differs");
        self.received=frame.serial;self.peer.live()?;
        let mut packet=self.pending_packet.take().unwrap();Ok(Some((frame.message,packet.fds.pop())))
    }
    fn receive_startup(&mut self,expected:Message)->Result<Option<OwnedFd>> {
        loop {self.budget.remaining()?;if let Some((message,fd))=self.poll()? {
            ensure!(message==expected,"machine bootstrap startup refused");self.budget.remaining()?;return Ok(fd);}
            std::thread::sleep(Duration::from_millis(2));}
    }
    fn receive_host_serving(&mut self)->Result<u32> {
        ensure!(self.host_ack.is_none(),"machine host acknowledgment already consumed");
        loop {self.budget.remaining()?;if let Some((message,fd))=self.poll()? {
            self.host_ack_fd=fd;
            let Message::HostServing(worker_tid)=message else {anyhow::bail!("machine actual worker acknowledgment absent");};
            // Preserve received DATA before deadline/shape checks. Unknown ACK
            // outcomes retain this gate and every original machine owner.
            self.host_ack=Some(worker_tid);
            ensure!(self.host_ack_fd.is_none() && worker_tid>1 && worker_tid!=self.peer.pid,"machine worker acknowledgment identity differs");
            self.budget.remaining()?;return Ok(worker_tid);
        }std::thread::sleep(Duration::from_millis(2));}
    }
}
fn single_original_main_thread()->Result<()> {
    ensure!(unsafe {libc::syscall(libc::SYS_gettid)} as u32==std::process::id(),"machine bootstrap must enter on original main thread");
    let mut tasks=fs::read_dir("/proc/self/task")?;
    ensure!(tasks.next().transpose()?.is_some() && tasks.next().transpose()?.is_none(),"machine bootstrap must precede all threads");Ok(())
}
fn caps(status:&str)->Result<u64> {Ok(u64::from_str_radix(status.lines().find_map(|l|l.strip_prefix("CapEff:")).context("machine capabilities absent")?.trim(),16)?)}
fn current_status()->Result<String> {Ok(String::from_utf8(storage::proc_bytes("/proc/thread-self/status",16384)?)?)}
fn zero_caps_and_groups(status:&str)->Result<()> {
    for field in ["CapEff:","CapPrm:","CapInh:","CapAmb:","CapBnd:"] {
        let value=status.lines().find_map(|l|l.strip_prefix(field)).context("machine capability floor absent")?.trim();
        ensure!(value.len()==16 && value.bytes().all(|b|b==b'0'),"machine capabilities remain");
    }
    ensure!(status.lines().any(|l|l.strip_prefix("Groups:").is_some_and(|v|v.trim().is_empty()))
        && status.lines().any(|l|l.strip_prefix("NoNewPrivs:").is_some_and(|v|v.trim()=="1")),"machine groups/NNP floor differs");Ok(())
}
fn bridge_identity(status:&str)->Result<()> {
    for field in ["Uid:","Gid:"] {let ids=status.lines().find_map(|l|l.strip_prefix(field)).context("machine bridge IDs absent")?;
        ensure!(ids.split_whitespace().eq(["0","0","0","0"]),"machine protected root IDs differ");}
    for (field,expected) in [("CapEff:",ENGINE_BOOTSTRAP_CAPS),("CapPrm:",ENGINE_BOOTSTRAP_CAPS),("CapBnd:",ENGINE_BOOTSTRAP_CAPS),("CapInh:",0),("CapAmb:",0)] {
        let actual=status.lines().find_map(|l|l.strip_prefix(field)).context("machine protected root capabilities absent")?.trim();
        ensure!(actual.len()==16 && u64::from_str_radix(actual,16)?==expected,"machine protected root capabilities differ");
    }
    ensure!(status.lines().any(|l|l.strip_prefix("Groups:").is_some_and(|v|v.trim().is_empty()))
        && status.lines().any(|l|l.strip_prefix("NoNewPrivs:").is_some_and(|v|v.trim()=="1"))
        && status.lines().any(|l|l.strip_prefix("TracerPid:").is_some_and(|v|v.trim()=="0")),"machine protected root groups/NNP/tracer differ");Ok(())
}
fn require_safe_dumpable_bytes(bytes:&[u8])->Result<()> {
    ensure!(bytes==b"0\n","machine credential drop requires host fs.suid_dumpable exactly0");Ok(())
}
/// Actual procfs policy and namespace custody across the credential transition.
/// The image-owned parent holds the original global MNT/USER namespaces. UID1500
/// cannot write this policy; the immutable root phase makes no policy writes and
/// the retained trusted host must keep its value0 throughout the transition.
struct PrivilegeDropPolicy {file:File,identity:(u64,u64),mnt:File,user:File}
impl PrivilegeDropPolicy {
    fn capture()->Result<Self> {
        let file=OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW|libc::O_CLOEXEC|libc::O_NONBLOCK).open(DUMPABLE_POLICY)?;
        let owner=Self {identity:storage::id(&file)?,file,mnt:storage::namespace(None,"mnt",libc::CLONE_NEWNS)?,
            user:storage::namespace(None,"user",libc::CLONE_NEWUSER)?};owner.check()?;Ok(owner)
    }
    fn check(&self)->Result<()> {
        let mut kind:libc::statfs=unsafe {std::mem::zeroed()};
        ensure!(unsafe {libc::fstatfs(self.file.as_raw_fd(),&mut kind)}==0 && kind.f_type==0x9fa0,"machine dumpable policy is not procfs");
        for m in [self.file.metadata()?,fs::symlink_metadata(DUMPABLE_POLICY)?] {
            ensure!(m.is_file() && !m.file_type().is_symlink() && (m.dev(),m.ino())==self.identity
                && m.uid()==0 && m.gid()==0 && m.mode() & 0o022==0,"machine dumpable policy custody changed");
        }
        for (held,name,kind) in [(&self.mnt,"mnt",libc::CLONE_NEWNS),(&self.user,"user",libc::CLONE_NEWUSER)] {
            ensure!(storage::id(held)?==storage::id(&storage::namespace(None,name,kind)?)?,"machine credential drop left original namespaces");
        }
        let mut bytes=[0u8;16];let n=self.file.read_at(&mut bytes,0)?;require_safe_dumpable_bytes(&bytes[..n])
    }
}
#[repr(C)]
struct CapHeader {version:u32,pid:i32}
#[repr(C)]
#[derive(Clone,Copy)]
struct CapData {effective:u32,permitted:u32,inheritable:u32}
unsafe fn clear_capabilities()->io::Result<()> {
    set_capabilities(0)
}
unsafe fn set_capabilities(mask:u64)->io::Result<()> {
    let header=CapHeader {version:0x2008_0522,pid:0};
    let data=[CapData {effective:mask as u32,permitted:mask as u32,inheritable:0},
        CapData {effective:(mask>>32) as u32,permitted:(mask>>32) as u32,inheritable:0}];
    if libc::syscall(libc::SYS_capset,&header as *const CapHeader,data.as_ptr())!=0 {return Err(io::Error::last_os_error());}Ok(())
}
unsafe fn restrict_bounding(mask:u64)->io::Result<()> {
    for cap in 0..64 {
        let present=libc::prctl(libc::PR_CAPBSET_READ,cap,0,0,0);
        if present<0 {if io::Error::last_os_error().raw_os_error()==Some(libc::EINVAL) {continue;}return Err(io::Error::last_os_error());}
        if mask & (1u64<<cap)==0 && libc::prctl(libc::PR_CAPBSET_DROP,cap,0,0,0)!=0 {return Err(io::Error::last_os_error());}
    }
    Ok(())
}
/// Async-signal-safe pre_exec syscalls only. All IDs remain0 across exec;
/// NNP must never be combined with the old mismatched real/effective IDs.
unsafe fn engine_exec_bridge()->io::Result<()> {
    if libc::setgroups(0,std::ptr::null())!=0 || libc::prctl(libc::PR_SET_KEEPCAPS,0,0,0,0)!=0
        || libc::prctl(libc::PR_CAP_AMBIENT,libc::PR_CAP_AMBIENT_CLEAR_ALL,0,0,0)!=0
        || libc::setresgid(0,0,0)!=0 || libc::setresuid(0,0,0)!=0 {return Err(io::Error::last_os_error());}
    restrict_bounding(ENGINE_BOOTSTRAP_CAPS)?;set_capabilities(ENGINE_BOOTSTRAP_CAPS)?;
    if libc::prctl(libc::PR_SET_NO_NEW_PRIVS,1,0,0,0)!=0 {return Err(io::Error::last_os_error());}
    if libc::prctl(libc::PR_SET_DUMPABLE,0,0,0,0)!=0 {return Err(io::Error::last_os_error());}Ok(())
}
fn protect_early_entry()->Result<()> {
    ensure!(unsafe {libc::prctl(libc::PR_SET_DUMPABLE,0,0,0,0)}==0
        && unsafe {libc::prctl(libc::PR_GET_DUMPABLE,0,0,0,0)}==0,"machine early nondumpable guard unavailable");Ok(())
}
/// FIRST binary routing hook, before argv/env/allocation/config. Raw identity/
/// capability queries only precede ND0. A protected or obsolete bridge claims quarantine
/// routing even if prctl fails; only enter_managed_machine_early authenticates.
pub fn claim_managed_machine_root_entry_first()->bool {
    let real=unsafe {libc::getuid()};let effective=unsafe {libc::geteuid()};
    let mut claimed=real==0 && effective==1500;
    if real==0 && effective==0 {
        let mut header=CapHeader {version:0x2008_0522,pid:0};let mut data=[CapData {effective:0,permitted:0,inheritable:0};2];
        let result=unsafe {libc::syscall(libc::SYS_capget,&mut header as *mut CapHeader,data.as_mut_ptr())};
        // Full-capability ordinary root invocations keep their legacy route.
        // A failed raw query claims quarantine instead of assuming no custody.
        claimed=result!=0 || (data[0].effective==ENGINE_BOOTSTRAP_CAPS as u32 && data[0].permitted==ENGINE_BOOTSTRAP_CAPS as u32
            && data[0].inheritable==0 && data[1].effective==0 && data[1].permitted==0 && data[1].inheritable==0);
    }
    if claimed {let _=unsafe {libc::prctl(libc::PR_SET_DUMPABLE,0,0,0,0)};}claimed
}
fn finish_engine_floor(policy:&mut Option<PrivilegeDropPolicy>)->Result<()> {
    protect_early_entry()?;bridge_identity(&current_status()?)?;
    ensure!(policy.is_none(),"machine credential drop policy already captured");*policy=Some(PrivilegeDropPolicy::capture()?);
    let policy=policy.as_ref().unwrap();
    // CAP_SETPCAP remains effective until ALL bounding bits have been dropped.
    unsafe {restrict_bounding(0)?;}
    policy.check()?;
    ensure!(unsafe {libc::setresgid(1500,1500,1500)}==0,"machine final GID drop failed");
    ensure!(unsafe {libc::prctl(libc::PR_GET_DUMPABLE,0,0,0,0)}==0,"machine GID drop changed safe dumpable policy");
    policy.check()?;
    // commit_creds publishes suid_dumpable0 before the effective/fs ID change;
    // the retained trusted host must not alter that global policy meanwhile.
    ensure!(unsafe {libc::setresuid(1500,1500,1500)}==0,"machine final UID drop failed");
    ensure!(unsafe {libc::prctl(libc::PR_GET_DUMPABLE,0,0,0,0)}==0,"machine UID drop changed safe dumpable policy");
    unsafe {clear_capabilities()?;}
    policy.check()?;protect_early_entry()?;
    storage::floor(std::process::id(),1500)?;zero_caps_and_groups(&current_status()?)
}
fn mapped_command(path:&str,args:&[&str],engine_bridge:bool,mapping:&[(&OwnedFd,i32)])->Result<(Command,Vec<OwnedFd>)> {
    let mut command=Command::new(path);command.args(args).env_clear().current_dir("/").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    let mut keep=Vec::new();let mut raw=Vec::new();let mut targets=std::collections::BTreeSet::new();
    for (fd,target) in mapping {ensure!((3..=7).contains(target) && targets.insert(*target),"machine inherited descriptor mapping invalid");
        let duplicate=unsafe {libc::fcntl(fd.as_raw_fd(),libc::F_DUPFD_CLOEXEC,20)};ensure!(duplicate>=20,"machine inherited descriptor duplication failed");
        keep.push(unsafe {OwnedFd::from_raw_fd(duplicate)});raw.push((duplicate,*target));}
    unsafe {command.pre_exec(move || {for &(from,to) in &raw {if libc::dup2(from,to)<0 {return Err(io::Error::last_os_error());}}
        if engine_bridge {engine_exec_bridge()?;}Ok(())});}Ok((command,keep))
}
struct ChildOwner {child:Child,process:Option<Process>,status:Option<ExitStatus>}
fn pin_original(child:&Child)->Result<Process> {
    let raw=unsafe {libc::syscall(libc::SYS_pidfd_open,child.id(),0)} as i32;
    ensure!(raw>=0,"machine original child pidfd unavailable");Process::capture(unsafe {OwnedFd::from_raw_fd(raw)})
}
enum SpawnEffect { DefiniteError, ChildStored }
fn spawn_retaining(slot:&mut Option<ChildOwner>,command:&mut Command)->Result<SpawnEffect> {
    ensure!(slot.is_none(),"machine original child already exists");
    // Pinned std Unix spawn Err means exec/setup failed and std reaped its
    // unpublished child. No injectable spawner/provider or replacement PID.
    let child=match command.spawn() {Ok(child)=>child,Err(_)=>return Ok(SpawnEffect::DefiniteError)};
    *slot=Some(ChildOwner {child,process:None,status:None});
    let owner=slot.as_mut().unwrap();owner.process=Some(pin_original(&owner.child)?);Ok(SpawnEffect::ChildStored)
}
impl ChildOwner {
    fn original(&self)->Result<&Process> {self.process.as_ref().context("machine original child pidfd capture uncertain")}
    fn live(&mut self)->Result<()> {
        if self.status.is_none() {self.status=self.child.try_wait()?;}
        ensure!(self.status.is_none(),"machine original child exited; explicit recovery required");self.original()?.live()
    }
}

const OBSERVATION_LIMIT:usize=4096;
#[derive(Clone,Copy,Debug,PartialEq,Eq)]
pub enum StartupObservationDelivery {NotPrepared,Delivered,Blocked,Unsupported,Failed,Uncertain,Unavailable}
struct StartupObservationOwner {
    bytes:Option<Vec<u8>>,output:Option<OwnedFd>,attempted:bool,delivery:StartupObservationDelivery,
    error:Option<anyhow::Error>,panic:Option<Box<dyn Any+Send>>,
}
impl StartupObservationOwner {
    fn new()->Self {Self {bytes:None,output:None,attempted:false,delivery:StartupObservationDelivery::NotPrepared,error:None,panic:None}}
}
#[derive(Serialize)]
struct ObservedId {device:String,inode:String}
fn observed_id(file:&File)->Result<ObservedId> {let (device,inode)=storage::id(file)?;Ok(ObservedId {device:device.to_string(),inode:inode.to_string()})}
#[derive(Serialize)]
struct ObservedCaps {effective:String,permitted:String,inheritable:String,ambient:String,bounding:String}
fn status_field<'a>(status:&'a str,field:&str)->Result<&'a str> {
    status.lines().find_map(|line|line.strip_prefix(field)).map(str::trim).context("machine observed kernel field absent")
}
fn observed_ids(status:&str,field:&str)->Result<[u32;4]> {
    let ids=status_field(status,field)?.split_whitespace().map(str::parse::<u32>).take(5).collect::<std::result::Result<Vec<_>,_>>()?;
    ensure!(ids.len()==4,"machine observed IDs differ");Ok([ids[0],ids[1],ids[2],ids[3]])
}
fn observed_caps(status:&str)->Result<ObservedCaps> {
    let read=|field|->Result<String> {let value=status_field(status,field)?;ensure!(value.len()==16,"machine observed capability field differs");
        u64::from_str_radix(value,16)?;Ok(value.into())};
    Ok(ObservedCaps {effective:read("CapEff:")?,permitted:read("CapPrm:")?,inheritable:read("CapInh:")?,ambient:read("CapAmb:")?,bounding:read("CapBnd:")?})
}
#[derive(Serialize)]
struct ObservedProcess {
    pid:u32,parent_pid:u32,image_path:&'static str,image_sha256:String,image_id:ObservedId,uid:[u32;4],gid:[u32;4],groups:Vec<u32>,
    caps:ObservedCaps,no_new_privs:u8,tracer_pid:u32,mnt:ObservedId,user:ObservedId,
}
fn observe_original_process(process:&Process,image:&Image,path:&'static str)->Result<ObservedProcess> {
    process.live()?;image.matches_process(process)?;
    let status=String::from_utf8(storage::proc_bytes(&format!("/proc/{}/status",process.pid),16384)?)?;
    ensure!(status_field(&status,"Pid:")?.parse::<u32>()?==process.pid && status_field(&status,"Tgid:")?.parse::<u32>()?==process.pid,
        "machine observed original process differs");
    let groups=status_field(&status,"Groups:")?.split_whitespace().map(str::parse::<u32>).take(33).collect::<std::result::Result<Vec<_>,_>>()?;
    ensure!(groups.len()<=32,"machine observed groups exceed bound");
    let no_new_privs=status_field(&status,"NoNewPrivs:")?.parse::<u8>()?;ensure!(no_new_privs<=1,"machine observed NNP invalid");
    let actual=File::open(format!("/proc/{}/exe",process.pid))?;
    let value=ObservedProcess {pid:process.pid,parent_pid:status_field(&status,"PPid:")?.parse()?,image_path:path,image_sha256:image.sha256.clone(),image_id:observed_id(&actual)?,
        uid:observed_ids(&status,"Uid:")?,gid:observed_ids(&status,"Gid:")?,groups,caps:observed_caps(&status)?,no_new_privs,
        tracer_pid:status_field(&status,"TracerPid:")?.parse()?,mnt:observed_id(&storage::namespace(Some(process.pid),"mnt",libc::CLONE_NEWNS)?)?,
        user:observed_id(&storage::namespace(Some(process.pid),"user",libc::CLONE_NEWUSER)?)?};
    image.matches_process(process)?;process.live()?;Ok(value)
}
fn boottime_stamp()->Result<String> {
    let mut value:libc::timespec=unsafe {std::mem::zeroed()};
    ensure!(unsafe {libc::clock_gettime(libc::CLOCK_BOOTTIME,&mut value)}==0 && value.tv_sec>=0 && (0..1_000_000_000).contains(&value.tv_nsec),
        "machine observation clock unavailable");Ok(((value.tv_sec as u128)*1_000_000_000+value.tv_nsec as u128).to_string())
}
#[derive(Serialize)]
struct StartupObservation {
    format_version:u32,kind:&'static str,scope:&'static str,nonce:String,original_constraint:FinalizationConstraint,
    observed_boottime_nanoseconds:String,boot_id:String,launcher:ObservedProcess,engine:ObservedProcess,service:ObservedProcess,
    host_worker_tid:u32,original_service_pidfd_and_exe_bound:bool,root_client_authenticated:bool,parent_authentication_accepted:bool,
    host_worker_ack_authenticated:bool,source_authority:bool,scheduler_ready:bool,normal_retirement_authority:bool,models:bool,
}
// One nonblocking pipe/socket write only. An unsafe SIGPIPE disposition makes
// the output unsupported; observability never changes process-wide signals.
// The fixed root launcher stays single-threaded and its immutable code does not
// change this disposition. Logging can neither block nor terminate the machine.
fn try_observation_write(fd:&OwnedFd,bytes:&[u8])->Result<StartupObservationDelivery> {
    ensure!(!bytes.is_empty() && bytes.len()<=OBSERVATION_LIMIT,"machine observation exceeds single-write bound");
    let mut stat:libc::stat=unsafe {std::mem::zeroed()};
    ensure!(unsafe {libc::fstat(fd.as_raw_fd(),&mut stat)}==0,"machine observation output metadata unavailable");
    if !matches!(stat.st_mode & libc::S_IFMT,libc::S_IFIFO|libc::S_IFSOCK) {return Ok(StartupObservationDelivery::Unsupported);}
    let mut action:libc::sigaction=unsafe {std::mem::zeroed()};
    ensure!(unsafe {libc::sigaction(libc::SIGPIPE,std::ptr::null(),&mut action)}==0,"machine observation SIGPIPE disposition unavailable");
    if action.sa_sigaction!=libc::SIG_IGN {return Ok(StartupObservationDelivery::Unsupported);}
    let flags=unsafe {libc::fcntl(fd.as_raw_fd(),libc::F_GETFL)};
    ensure!(flags>=0 && flags & libc::O_ACCMODE!=libc::O_RDONLY,"machine observation output is not writable");
    ensure!(unsafe {libc::fcntl(fd.as_raw_fd(),libc::F_SETFL,flags|libc::O_NONBLOCK)}==0,"machine observation nonblocking output unavailable");
    let n=unsafe {libc::write(fd.as_raw_fd(),bytes.as_ptr().cast(),bytes.len())};let error=io::Error::last_os_error();
    let restored=unsafe {libc::fcntl(fd.as_raw_fd(),libc::F_SETFL,flags)};
    ensure!(restored==0,"machine observation output restoration uncertain");
    if n==bytes.len() as isize {return Ok(StartupObservationDelivery::Delivered);}
    if n<0 && error.kind()==io::ErrorKind::WouldBlock {return Ok(StartupObservationDelivery::Blocked);}
    if n<0 && error.kind()!=io::ErrorKind::Interrupted {return Ok(StartupObservationDelivery::Failed);}
    Ok(StartupObservationDelivery::Uncertain)
}

#[derive(Clone,Copy,PartialEq,Eq)]
enum Stage { SpawnEngine, WaitEngine, SpawnService, SendService, WaitClient, WaitHostServing, Established, Quarantined }
/// Actual original machine process/FD/image custody. No Clone/Serde, no Drop
/// cleanup. Keep outside catch_unwind and retain after every uncertain result.
#[must_use="retain actual children and descriptors through explicit existing-owner drain or VM teardown"]
pub struct MachineRootBootstrap {
    original:FinalizationBudget,data:BootstrapData,own:Process,launcher_image:Image,engine_image:Image,service_image:Image,
    mnt:File,user:File,boot:Vec<u8>,root_caps:u64,drop_policy:PrivilegeDropPolicy,lock:Option<File>,lock_id:Option<(u64,u64)>,
    launcher_exe:OwnedFd,data_fd:OwnedFd,service_endpoint:OwnedFd,engine_endpoint:Option<OwnedFd>,
    gate_parent:Option<OwnedFd>,gate_child:Option<OwnedFd>,gate:Option<Gate>,
    engine:Option<ChildOwner>,service:Option<ChildOwner>,service_exe:Option<OwnedFd>,stage:Stage,
    panic:Option<Box<dyn Any+Send>>,retryable_spawn_error:bool,host_worker_tid:Option<u32>,observation:StartupObservationOwner,
}
impl MachineRootBootstrap {
    /// Read-only image/process checks plus fresh root lock/anonymous FD setup.
    /// The original budget must be captured BEFORE the bootstrap is queued.
    pub fn capture(original:FinalizationBudget)->Result<Self> {
        original.remaining()?;single_original_main_thread()?;
        ensure!(unsafe {libc::getuid()}==0 && unsafe {libc::geteuid()}==0 && unsafe {libc::getgid()}==0 && unsafe {libc::getegid()}==0,
            "machine root requires actual UID/GID0");storage::floor(std::process::id(),0)?;
        let root_caps=caps(&current_status()?)?;
        ensure!(root_caps & ((1<<6)|(1<<7)|(1<<8)|(1<<19)|(1<<21))==((1<<6)|(1<<7)|(1<<8)|(1<<19)|(1<<21)),
            "machine root lacks SETGID/SETUID/SETPCAP/SYS_PTRACE/SYS_ADMIN");
        let own=Process::current()?;let launcher_image=Image::capture(Path::new(LAUNCHER_PATH))?;launcher_image.matches_process(&own)?;
        let launcher_exe:OwnedFd=File::open(format!("/proc/{}/exe",own.pid))?.into();storage::fd_cloexec(&launcher_exe)?;
        launcher_image.matches_fd(&launcher_exe)?;launcher_image.matches_process(&own)?;
        let data=BootstrapData {version:1,nonce:uuid::Uuid::new_v4().to_string(),constraint:original.constraint()};
        let data_fd=sealed_data(&data)?;let (service_endpoint,engine_endpoint)=storage::bootstrap_socket_pair()?;
        let (gate_parent,gate_child)=storage::bootstrap_socket_pair()?;
        let mut owner=Self {original,data,own,launcher_image,engine_image:Image::capture(Path::new(ENGINE_PATH))?,
            service_image:Image::capture(Path::new(SERVICE_PATH))?,mnt:storage::namespace(None,"mnt",libc::CLONE_NEWNS)?,
            user:storage::namespace(None,"user",libc::CLONE_NEWUSER)?,boot:storage::proc_bytes("/proc/sys/kernel/random/boot_id",64)?,
            root_caps,drop_policy:PrivilegeDropPolicy::capture()?,lock:None,lock_id:None,launcher_exe,data_fd,service_endpoint,engine_endpoint:Some(engine_endpoint),
            gate_parent:Some(gate_parent),gate_child:Some(gate_child),gate:None,engine:None,service:None,service_exe:None,
            stage:Stage::SpawnEngine,panic:None,retryable_spawn_error:false,host_worker_tid:None,observation:StartupObservationOwner::new()};
        owner.root_current()?;
        ensure!(Path::new("/run").canonicalize()?==Path::new("/run"),"machine /run redirects");
        let run=fs::symlink_metadata("/run")?;ensure!(run.is_dir() && run.uid()==0 && run.gid()==0 && run.mode() & 0o022==0,"machine /run not root controlled");
        ensure!(fs::symlink_metadata("/run/rudder-root-storage").err().is_some_and(|e|e.kind()==io::ErrorKind::NotFound),
            "machine root storage already exists; explicit prior-owner recovery required");
        // A stale path never authorizes a new machine. Even after this owner
        // exits the name stays until explicit recovery; no TTL/restart adoption.
        owner.original.remaining()?;
        owner.lock=Some(OpenOptions::new().read(true).write(true).create_new(true).mode(0o600)
            .custom_flags(libc::O_NOFOLLOW|libc::O_CLOEXEC).open("/run/rudder-machine-root.lock")?);
        owner.lock_id=Some(storage::id(owner.lock.as_ref().unwrap())?);
        fs2::FileExt::try_lock_exclusive(owner.lock.as_ref().unwrap())?;owner.root_current()?;owner.original.remaining()?;Ok(owner)
    }
    fn root_current(&self)->Result<()> {
        ensure!(self.own.pid==std::process::id() && unsafe {libc::getuid()}==0 && unsafe {libc::geteuid()}==0
            && unsafe {libc::getgid()}==0 && unsafe {libc::getegid()}==0,"machine original root identity changed");
        self.own.live()?;storage::floor(self.own.pid,0)?;self.launcher_image.matches_process(&self.own)?;
        self.engine_image.check()?;self.service_image.check()?;
        ensure!(caps(&current_status()?)?==self.root_caps,"machine root capability custody changed");
        self.drop_policy.check()?;
        for (held,name,kind) in [(&self.mnt,"mnt",libc::CLONE_NEWNS),(&self.user,"user",libc::CLONE_NEWUSER)] {
            ensure!(storage::id(held)?==storage::id(&storage::namespace(None,name,kind)?)?
                && storage::id(held)?==storage::id(&storage::namespace(Some(1),name,kind)?)?,"machine root left original global namespace");
        }
        ensure!(self.boot==storage::proc_bytes("/proc/sys/kernel/random/boot_id",64)?,"machine root boot changed");
        if let Some(lock)=&self.lock {for m in [lock.metadata()?,fs::symlink_metadata("/run/rudder-machine-root.lock")?] {
            ensure!(m.is_file() && !m.file_type().is_symlink() && Some((m.dev(),m.ino()))==self.lock_id
                && m.uid()==0 && m.gid()==0 && m.nlink()==1 && m.mode() & 0o7777==0o600,"machine root lock custody changed");}}
        Ok(())
    }
    fn engine_current(&mut self)->Result<()> {
        let child=self.engine.as_mut().context("machine original engine absent")?;child.live()?;
        let process=child.original()?;self.engine_image.matches_process(process)?;storage::floor(process.pid,1500)?;
        let status=String::from_utf8(storage::proc_bytes(&format!("/proc/{}/status",process.pid),16384)?)?;
        ensure!(status.lines().any(|l|l.strip_prefix("TracerPid:").is_some_and(|v|v.trim()=="0")),"machine engine has a tracer");
        ensure!(storage::id(&self.mnt)?==storage::id(&storage::namespace(Some(process.pid),"mnt",libc::CLONE_NEWNS)?)?
            && storage::id(&self.user)?==storage::id(&storage::namespace(Some(process.pid),"user",libc::CLONE_NEWUSER)?)?,"machine engine entered guest namespaces");Ok(())
    }
    /// Borrowing retry: only ordinary pinned std Unix returned spawn Err leaves
    /// its uncreated stage retryable, under this SAME original deadline. There
    /// is no automatic retry; any known/ambiguous effect remains quarantined.
    pub fn start_retaining(&mut self)->Result<()> {
        if self.stage==Stage::Established {return self.poll_lifetime_retaining();}
        self.retryable_spawn_error=false;
        let result=std::panic::catch_unwind(std::panic::AssertUnwindSafe(||self.start_inner()));
        match result {
            Ok(Ok(()))=>Ok(()),
            Ok(Err(_))=>{if !self.retryable_spawn_error {self.quarantine();}
                Err(anyhow::anyhow!("machine startup failed; original carriers retained"))}
            Err(panic)=>{self.panic=Some(panic);self.quarantine();Err(anyhow::anyhow!("machine startup panicked; original carriers retained"))}
        }
    }
    fn start_inner(&mut self)->Result<()> {
        loop {self.original.remaining()?;self.root_current()?;
            match self.stage {
                Stage::SpawnEngine=>{
                    let (mut command,held)=mapped_command(ENGINE_PATH,&["engine","managed-machine"],true,&[
                        (self.engine_endpoint.as_ref().unwrap(),3),(self.gate_child.as_ref().unwrap(),4),(self.own.fd(),5),(&self.launcher_exe,6),(&self.data_fd,7)])?;
                    self.original.remaining()?;
                    match spawn_retaining(&mut self.engine,&mut command)? {
                        SpawnEffect::DefiniteError=>{self.retryable_spawn_error=true;anyhow::bail!("machine engine returned ordinary spawn error");}
                        SpawnEffect::ChildStored=>{drop(held);self.stage=Stage::WaitEngine;}
                    }
                    self.gate=Some(Gate::capture(self.gate_parent.as_ref().unwrap().try_clone()?,self.engine.as_ref().unwrap().original()?.fd().try_clone()?,1500,
                        self.data.clone(),self.original.clone())?);
                    self.engine_endpoint.take();self.gate_child.take();self.gate_parent.take();
                }
                Stage::WaitEngine=>{
                    ensure!(self.gate.as_mut().unwrap().receive_startup(Message::EngineWaiting)?.is_none(),"machine early engine sent extra descriptor");
                    self.engine_current()?;self.stage=Stage::SpawnService;
                }
                Stage::SpawnService=>{
                    self.engine_current()?;
                    let (mut command,held)=mapped_command(SERVICE_PATH,&[],false,&[(&self.service_endpoint,3),(self.engine.as_ref().unwrap().original()?.fd(),4)])?;
                    self.original.remaining()?;
                    match spawn_retaining(&mut self.service,&mut command)? {
                        SpawnEffect::DefiniteError=>{self.retryable_spawn_error=true;anyhow::bail!("machine service returned ordinary spawn error");}
                        SpawnEffect::ChildStored=>{drop(held);self.stage=Stage::SendService;}
                    }
                }
                Stage::SendService=>{
                    self.engine_current()?;let service=self.service.as_mut().unwrap();service.live()?;
                    let process=service.original()?;self.service_image.matches_process(process)?;
                    if self.service_exe.is_none() {self.service_exe=Some(File::open(format!("/proc/{}/exe",process.pid))?.into());}
                    let actual=self.service_exe.as_ref().unwrap();storage::fd_cloexec(actual)?;
                    self.service_image.matches_fd(actual)?;self.service_image.matches_process(process)?;process.live()?;
                    self.gate.as_mut().unwrap().send_startup(Message::ServicePidfd,Some(process.fd()))?;
                    self.gate.as_mut().unwrap().send_startup(Message::ServiceImage,Some(actual))?;
                    self.stage=Stage::WaitClient;
                }
                Stage::WaitClient=>{
                    ensure!(self.gate.as_mut().unwrap().receive_startup(Message::ClientAuthenticated)?.is_none(),"machine client ACK sent extra descriptor");
                    self.engine_current()?;self.service.as_mut().unwrap().live()?;self.root_current()?;self.original.remaining()?;
                    self.gate.as_mut().unwrap().send_startup(Message::ParentAccepted,None)?;
                    self.stage=Stage::WaitHostServing;
                }
                Stage::WaitHostServing=>{
                    let tid=self.gate.as_mut().unwrap().receive_host_serving()?;self.host_worker_tid=Some(tid);
                    self.engine_current()?;self.service.as_mut().unwrap().live()?;self.root_current()?;
                    self.original.remaining()?;self.stage=Stage::Established;return Ok(());
                }
                Stage::Established=>return Ok(()),
                Stage::Quarantined=>anyhow::bail!("machine original owner quarantined"),
            }
        }
    }
    /// OFF App original-child/image/namespace observation, never helper/backend
    /// drain. A loss is absorbing and cannot create a replacement machine.
    pub fn poll_lifetime_retaining(&mut self)->Result<()> {
        let result=(|| ->Result<()> {ensure!(self.stage==Stage::Established,"machine startup not established");self.root_current()?;self.engine_current()?;
            let service=self.service.as_mut().unwrap();service.live()?;self.service_image.matches_process(service.original()?)?;
            ensure!(self.gate.as_mut().unwrap().poll()?.is_none(),"machine unexpected post-bootstrap frame");Ok(())})();
        if result.is_err() {self.quarantine();}result
    }
    fn startup_observation_bytes(&mut self)->Result<Vec<u8>> {
        ensure!(self.stage==Stage::Established,"machine actual worker startup not established");self.original.remaining()?;
        self.root_current()?;self.engine_current()?;let service=self.service.as_mut().unwrap();service.live()?;
        let engine=self.engine.as_ref().unwrap().original()?;let service=self.service.as_ref().unwrap().original()?;
        let tid=self.host_worker_tid.context("machine actual worker acknowledgment absent")?;
        // Thread ID is authenticated DATA, never a pidfd or JoinHandle. Read
        // only beneath the held original engine, without enrolling/reopening it.
        let worker=String::from_utf8(storage::proc_bytes(&format!("/proc/{}/task/{tid}/status",engine.pid),16384)?)?;
        ensure!(tid!=engine.pid && status_field(&worker,"Pid:")?.parse::<u32>()?==tid
            && status_field(&worker,"Tgid:")?.parse::<u32>()?==engine.pid && observed_ids(&worker,"Uid:")?==[1500;4]
            && observed_ids(&worker,"Gid:")?==[1500;4],"machine acknowledged worker no longer belongs to original engine");
        zero_caps_and_groups(&worker)?;
        let observation=StartupObservation {format_version:1,kind:"rudder-machine-root-bootstrap-observation",
            scope:"production-native-root-launcher-client-startup-only",nonce:self.data.nonce.clone(),original_constraint:self.original.constraint(),
            observed_boottime_nanoseconds:boottime_stamp()?,boot_id:String::from_utf8(self.boot.clone())?.trim().into(),
            launcher:observe_original_process(&self.own,&self.launcher_image,LAUNCHER_PATH)?,
            engine:observe_original_process(engine,&self.engine_image,ENGINE_PATH)?,service:observe_original_process(service,&self.service_image,SERVICE_PATH)?,
            host_worker_tid:tid,original_service_pidfd_and_exe_bound:true,root_client_authenticated:true,parent_authentication_accepted:true,
            host_worker_ack_authenticated:true,source_authority:false,scheduler_ready:false,normal_retirement_authority:false,models:false};
        self.service_image.matches_fd(self.service_exe.as_ref().context("machine original service executable witness absent")?)?;
        self.root_current()?;self.engine_current()?;self.service.as_mut().unwrap().live()?;self.original.remaining()?;
        let mut bytes=canonical_json(&observation)?;ensure!(bytes.len()<OBSERVATION_LIMIT,"machine observation exceeds bound");bytes.push(b'\n');Ok(bytes)
    }
    /// One best-effort observation attempt; delivery never affects machine
    /// authority, admission or supervision. No retries or new time budget.
    /// Original owners, bytes, actual output FD and unknown outcomes stay held.
    pub fn observe_startup_best_effort(&mut self) {
        if self.observation.attempted {return;}self.observation.attempted=true;
        let result=std::panic::catch_unwind(std::panic::AssertUnwindSafe(||->Result<StartupObservationDelivery> {
            self.observation.bytes=Some(self.startup_observation_bytes()?);
            let raw=unsafe {libc::fcntl(libc::STDOUT_FILENO,libc::F_DUPFD_CLOEXEC,20)};
            ensure!(raw>=20,"machine observation original stdout unavailable");
            self.observation.output=Some(unsafe {OwnedFd::from_raw_fd(raw)});
            self.observation.delivery=StartupObservationDelivery::Uncertain;
            try_observation_write(self.observation.output.as_ref().unwrap(),self.observation.bytes.as_ref().unwrap())
        }));
        match result {
            Ok(Ok(delivery))=>self.observation.delivery=delivery,
            Ok(Err(error))=>{if self.observation.delivery!=StartupObservationDelivery::Uncertain {self.observation.delivery=StartupObservationDelivery::Unavailable;}
                self.observation.error=Some(error);},
            Err(panic)=>{self.observation.delivery=StartupObservationDelivery::Uncertain;self.observation.panic=Some(panic);}
        }
    }
    /// Borrowed observation DATA only. A complete line is not live ownership,
    /// scheduling/source admission, worker drain or normal retirement authority.
    pub fn startup_observation_json(&self)->Option<&[u8]> {self.observation.bytes.as_deref()}
    pub fn startup_observation_delivery(&self)->StartupObservationDelivery {self.observation.delivery}
    pub fn quarantine(&mut self) {
        if self.stage==Stage::Quarantined {return;}
        self.stage=Stage::Quarantined;self.retryable_spawn_error=false;self.original.cancel();if let Some(gate)=&mut self.gate {gate.try_quarantine();}
    }
    pub fn cancel_startup_retaining(&mut self) {self.quarantine();}
}

/// Actual engine-side launcher/service/client custody. It grants no project
/// lease, scheduler, source admission, backend readiness or host drain proof.
#[must_use="retain actual bootstrap and Root client owners for the machine lifetime"]
pub struct ManagedMachineBootstrap {
    raw:[Option<OwnedFd>;5],own:Option<Process>,engine_image:Option<Image>,launcher_image:Option<Image>,
    service_image:Option<Image>,service_pidfd:Option<OwnedFd>,service_exe:Option<OwnedFd>,service:Option<Process>,
    mnt:Option<File>,user:Option<File>,boot:Vec<u8>,drop_policy:Option<PrivilegeDropPolicy>,data:Option<BootstrapData>,original:Option<FinalizationBudget>,
    gate:Option<Gate>,client:Option<RootStorageClient>,parent_accepted:bool,host_ack:HostServingAck,quarantined:bool,panic:Option<Box<dyn Any+Send>>,
}
#[derive(Clone,Copy)]
enum HostServingAck {Pending,Sending(u32),Sent(u32),Uncertain(u32)}
/// Failure owns the ACTUAL partial engine-side carrier. Display/Debug never
/// prints its fields. Main must retain this value instead of falling through.
pub struct ManagedMachineBootstrapFailure {owner:Box<ManagedMachineBootstrap>,_reason:anyhow::Error}
impl fmt::Debug for ManagedMachineBootstrapFailure {fn fmt(&self,f:&mut fmt::Formatter<'_>)->fmt::Result {f.write_str("managed machine bootstrap quarantined; actual owners retained")}}
impl fmt::Display for ManagedMachineBootstrapFailure {fn fmt(&self,f:&mut fmt::Formatter<'_>)->fmt::Result {fmt::Debug::fmt(self,f)}}
impl std::error::Error for ManagedMachineBootstrapFailure {}
impl ManagedMachineBootstrapFailure {
    pub fn retain_quarantined(self)->! {eprintln!("MANAGED_MACHINE_BOOTSTRAP_QUARANTINED");
        loop {std::hint::black_box(&self.owner);std::thread::sleep(Duration::from_secs(1));}}
}
impl ManagedMachineBootstrap {
    fn empty()->Self {Self {raw:std::array::from_fn(|_|None),own:None,engine_image:None,launcher_image:None,service_image:None,
        service_pidfd:None,service_exe:None,service:None,mnt:None,user:None,boot:Vec::new(),drop_policy:None,data:None,original:None,gate:None,client:None,
        parent_accepted:false,host_ack:HostServingAck::Pending,
        quarantined:false,panic:None}}
    fn enter_inner(&mut self)->Result<()> {
        // These syscalls must be the first native Main action. No argument/env
        // configuration, user extension, profile or thread may precede the floor.
        protect_early_entry()?;single_original_main_thread()?;finish_engine_floor(&mut self.drop_policy)?;
        let args:Vec<_>=std::env::args_os().take(4).collect();
        ensure!(args.len()==3 && args[1]=="engine" && args[2]=="managed-machine" && std::env::vars_os().next().is_none(),
            "machine engine requires fixed early argv and cleared environment");
        for fd in 3..=7 {ensure!(unsafe {libc::fcntl(fd,libc::F_GETFD)}>=0,"machine original bootstrap descriptor absent");}
        for (index,fd) in (3..=7).enumerate() {self.raw[index]=Some(unsafe {OwnedFd::from_raw_fd(fd)});storage::fd_cloexec(self.raw[index].as_ref().unwrap())?;}
        self.data=Some(read_data(self.raw[4].as_ref().unwrap())?);
        self.original=Some(FinalizationBudget::from_constraint(&self.data.as_ref().unwrap().constraint)?);
        self.own=Some(Process::current()?);self.engine_image=Some(Image::capture(Path::new(ENGINE_PATH))?);
        self.engine_image.as_ref().unwrap().matches_process(self.own.as_ref().unwrap())?;
        self.original.as_ref().unwrap().remaining()?;
        self.launcher_image=Some(Image::capture(Path::new(LAUNCHER_PATH))?);
        let image=self.raw[3].as_ref().unwrap();
        ensure!(unsafe {libc::fcntl(image.as_raw_fd(),libc::F_GETFL)} & libc::O_ACCMODE==libc::O_RDONLY,"machine original launcher exe witness is not readonly");
        self.launcher_image.as_ref().unwrap().matches_fd(image)?;
        self.mnt=Some(storage::namespace(None,"mnt",libc::CLONE_NEWNS)?);self.user=Some(storage::namespace(None,"user",libc::CLONE_NEWUSER)?);
        self.boot=storage::proc_bytes("/proc/sys/kernel/random/boot_id",64)?;
        self.gate=Some(Gate::capture(self.raw[1].as_ref().unwrap().try_clone()?,self.raw[2].as_ref().unwrap().try_clone()?,0,
            self.data.as_ref().unwrap().clone(),self.original.as_ref().unwrap().clone())?);
        self.gate.as_mut().unwrap().send_startup(Message::EngineWaiting,None)?;
        self.service_pidfd=Some(self.gate.as_mut().unwrap().receive_startup(Message::ServicePidfd)?.context("machine original service pidfd absent")?);
        self.service_exe=Some(self.gate.as_mut().unwrap().receive_startup(Message::ServiceImage)?.context("machine actual service exe witness absent")?);
        self.service=Some(Process::capture(self.service_pidfd.as_ref().unwrap().try_clone()?)?);
        self.service_image=Some(Image::capture(Path::new(SERVICE_PATH))?);self.service_image.as_ref().unwrap().matches_fd(self.service_exe.as_ref().unwrap())?;
        self.original.as_ref().unwrap().remaining()?;
        self.client=Some(RootStorageClient::from_bootstrap(self.raw[0].as_ref().unwrap().try_clone()?,
            self.service_pidfd.as_ref().unwrap().try_clone()?,self.service_exe.as_ref().unwrap().try_clone()?)?);
        self.client.as_mut().unwrap().authenticate_original_service(self.original.as_ref().unwrap())?;
        self.gate.as_mut().unwrap().send_startup(Message::ClientAuthenticated,None)?;
        ensure!(self.gate.as_mut().unwrap().receive_startup(Message::ParentAccepted)?.is_none(),"machine parent sent extra acceptance descriptor");
        self.parent_accepted=true;
        self.check_current_retaining()
    }
    /// OFF App. This observes original physical custody, never process/actor
    /// drain. Root Main must poll it before admitting later managed work.
    pub fn check_current_retaining(&mut self)->Result<()> {
        let result=(|| ->Result<()> {
            ensure!(!self.quarantined && self.client.is_some(),"machine actual client is quarantined/unavailable");
            let own=self.own.as_ref().context("machine original self pidfd absent")?;
            ensure!(own.pid==std::process::id(),"machine client moved to another process");storage::floor(own.pid,1500)?;own.live()?;
            ensure!(unsafe {libc::prctl(libc::PR_GET_DUMPABLE,0,0,0,0)}==0,"machine nondumpable guard changed");
            self.engine_image.as_ref().unwrap().matches_process(own)?;self.launcher_image.as_ref().unwrap().check()?;
            self.launcher_image.as_ref().unwrap().matches_fd(self.raw[3].as_ref().unwrap())?;
            self.service_image.as_ref().unwrap().check()?;self.service.as_ref().unwrap().live()?;
            ensure!(storage::id(self.mnt.as_ref().unwrap())?==storage::id(&storage::namespace(None,"mnt",libc::CLONE_NEWNS)?)?
                && storage::id(self.user.as_ref().unwrap())?==storage::id(&storage::namespace(None,"user",libc::CLONE_NEWUSER)?)?
                && self.boot==storage::proc_bytes("/proc/sys/kernel/random/boot_id",64)?,"machine engine left original host namespace/boot");
            ensure!(self.gate.as_mut().unwrap().poll()?.is_none(),"machine root requested quarantine or sent an unexpected frame");Ok(())
        })();
        if result.is_err() && !self.quarantined {self.quarantined=true;if let Some(gate)=&mut self.gate {gate.try_quarantine();}}result
    }
    /// Actual authenticated client owner, borrowed while this retained machine
    /// carrier remains current. No raw FD, path or serialized receipt accessor.
    pub fn client_mut_retaining(&mut self)->Result<&mut RootStorageClient> {self.check_current_retaining()?;Ok(self.client.as_mut().unwrap())}
    /// Call ONLY from the actual native machine worker, after its original
    /// JoinHandle is retained by Main and the worker has acquired that latch.
    /// This validates the held client and sends one authenticated startup fact;
    /// it creates no scheduler/source/drain authority. Original30s budget only.
    pub fn acknowledge_host_serving_retaining(&mut self)->Result<()> {
        let result=(||->Result<()> {
            let replay=match self.host_ack {
                HostServingAck::Pending=>false,
                HostServingAck::Sent(tid)=>{ensure!(tid>1 && tid!=std::process::id(),"machine cached worker acknowledgment identity differs");true},
                HostServingAck::Sending(tid)|HostServingAck::Uncertain(tid)=>anyhow::bail!("machine original worker acknowledgment uncertain for TID {tid}"),
            };
            if !replay {self.original.as_ref().context("machine original startup budget absent")?.remaining()?;}
            ensure!(self.parent_accepted,"machine original parent acceptance absent");self.check_current_retaining()?;
            self.client.as_ref().context("machine actual Root client absent")?.assert_authenticated_bootstrap_current()?;
            if replay {return Ok(());}
            let tid=unsafe {libc::syscall(libc::SYS_gettid)};
            ensure!(tid>1 && tid as u32!=std::process::id(),"machine host acknowledgment requires actual non-main worker");
            let tid=u32::try_from(tid)?;self.host_ack=HostServingAck::Sending(tid);
            self.gate.as_mut().unwrap().send_startup(Message::HostServing(tid),None)?;
            self.host_ack=HostServingAck::Sent(tid);Ok(())
        })();
        if result.is_err() {
            if let HostServingAck::Sending(tid)=self.host_ack {self.host_ack=HostServingAck::Uncertain(tid);}
            self.quarantined=true;if let Some(gate)=&mut self.gate {gate.try_quarantine();}
        }result
    }
}

/// Invoke only as the FIRST action of the new fixed Main command, before
/// threads/config/App. Failure RETURNS its actual carrier; never drop it or
/// fall through to legacy/default launch. No public image/FD-number override.
pub fn enter_managed_machine_early()->std::result::Result<ManagedMachineBootstrap,ManagedMachineBootstrapFailure> {
    let protection=protect_early_entry();
    static TAKEN:AtomicBool=AtomicBool::new(false);
    let mut owner=ManagedMachineBootstrap::empty();
    let result=if let Err(error)=protection {Err(error)}
        else if TAKEN.swap(true,Ordering::AcqRel) {Err(anyhow::anyhow!("machine inherited bootstrap already consumed"))}
        else {match std::panic::catch_unwind(std::panic::AssertUnwindSafe(||owner.enter_inner())) {
            Ok(result)=>result,Err(panic)=>{owner.panic=Some(panic);Err(anyhow::anyhow!("machine early bootstrap panicked"))}}};
    match result {Ok(())=>Ok(owner),Err(reason)=>{owner.quarantined=true;if let Some(gate)=&mut owner.gate {gate.try_quarantine();}
        Err(ManagedMachineBootstrapFailure {owner:Box::new(owner),_reason:reason})}}
}

/// Fixed root binary entry. Startup/physical loss never starts a replacement or
/// kills/cleans a service. Retention remains until explicit owner drain/VM stop.
pub fn run_machine_root()->Result<()> {
    ensure!(std::env::args_os().count()==1 && std::env::vars_os().next().is_none(),"machine root requires fixed argv and cleared environment");
    let original=FinalizationBudget::begin(Duration::from_secs(30))?;
    let mut owner=MachineRootBootstrap::capture(original)?;
    if owner.start_retaining().is_err() {owner.quarantine();eprintln!("MACHINE_ROOT_STARTUP_QUARANTINED");}
    else {owner.observe_startup_best_effort();}
    loop {
        if owner.stage==Stage::Established {
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(||owner.poll_lifetime_retaining())) {
                Ok(Ok(()))=>{},Ok(Err(_))=>eprintln!("MACHINE_ROOT_LIFETIME_QUARANTINED"),
                Err(panic)=>{owner.panic=Some(panic);owner.quarantine();eprintln!("MACHINE_ROOT_LIFETIME_QUARANTINED");}
            }
        }
        std::hint::black_box(&owner);std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(test)]
#[path="root_storage_bootstrap_tests.rs"]
mod tests;
