//! Live image-service FD capsules. Serialized fields describe original kernel
//! owners; only a packet from the actual fixed Root service can issue a grant.
//! No path constructor, uid0 constructor, saved receipt or LocalStore pin grant.
use super::*;
use super::finalization_budget::{FinalizationBudget,FinalizationConstraint};
use anyhow::{bail,Context};
use crate::protocol::{canonical_json,ObjectRef};
use serde::{Deserialize,Serialize};
use sha2::{Digest,Sha256};
use std::{fs::{self,File,OpenOptions},io,os::{fd::{AsRawFd,FromRawFd,OwnedFd,RawFd},unix::fs::{FileExt,MetadataExt,OpenOptionsExt}},sync::atomic::{AtomicBool,Ordering},time::Duration};

pub const SERVICE_IMAGE:&str="/usr/local/bin/rudder-root-storage";
pub const LAUNCHER_IMAGE:&str="/usr/local/bin/rudder-machine-root";
pub const ENGINE_IMAGE:&str="/opt/rudder/dist/native/rudder-native";
pub const MAX_RUNTIME_COMPONENTS:usize=9;
pub const RUNTIME_FIXED_FDS:usize=14;
pub const MAX_RUNTIME_FDS:usize=RUNTIME_FIXED_FDS+MAX_RUNTIME_COMPONENTS;
const SEALED:i32=libc::F_SEAL_WRITE|libc::F_SEAL_GROW|libc::F_SEAL_SHRINK|libc::F_SEAL_SEAL;
const MAX_FRAME:usize=24*1024;
/// Exact installer entries and approved guest projections. No public /run
/// prefix is introduced. Private Root mount pathnames never enter the spec.
pub const COMPONENTS:[(&str,&str,&str,bool);9]=[
    ("usr","/usr","/usr",true),
    ("package","/opt/rudder","/opt/runtime/rudder",true),
    ("model","/opt/rudder-managed-runtime/v1","/opt/runtime/model/v1",true),
    ("certificates","/etc/ssl/certs","/etc/ssl/certs",true),
    ("alternatives","/etc/alternatives","/etc/alternatives",true),
    ("loader-cache","/etc/ld.so.cache","/etc/ld.so.cache",false),
    ("timezone","/etc/localtime","/etc/localtime",false),
    ("ssl-config","/etc/ssl/openssl.cnf","/etc/ssl/openssl.cnf",false),
    ("python-site","/etc/python3.11/sitecustomize.py","/etc/python3.11/sitecustomize.py",false),
];
#[derive(Clone,Debug,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeFdData {pub device:u64,pub inode:u64,pub mount_id:u64,pub directory:bool}
#[derive(Clone,Debug,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeExecutableData {pub installer_path:String,pub sha256:String,pub size_bytes:u64}
/// DATA only. The sealed original Root copy is additionally compared byte for
/// byte and every descriptor has to match the live original service packet.
#[derive(Clone,Debug,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeImageCapsuleData {
    pub version:u32,pub request_nonce:String,pub binding_sha256:String,pub engine_instance_id:String,pub epoch:String,
    pub config_sha256:String,pub registration_proof_sha256:String,pub boot_id:String,
    pub launcher_pid:u32,pub service_pid:u32,pub engine_pid:u32,pub helper_pid:u32,pub helper_start_ticks:String,
    pub launcher_sha256:String,pub service_sha256:String,pub engine_sha256:String,
    pub inventory:ObjectRef,pub manifest:ObjectRef,pub installer_input:ObjectRef,pub constraint:FinalizationConstraint,
    pub floor:RuntimeFdData,pub components:Vec<RuntimeFdData>,pub selected_executables:Vec<RuntimeExecutableData>,
}
fn hash_text(value:&str)->Result<()> {ensure!(value.len()==64&&value.bytes().all(|b|b.is_ascii_digit()||(b'a'..=b'f').contains(&b)),"runtime SHA invalid");Ok(())}
fn uuid(value:&str)->Result<()> {ensure!(Uuid::parse_str(value).is_ok_and(|u|u.get_version_num()==4&&u.to_string()==value),"runtime nonce invalid");Ok(())}
impl RuntimeImageCapsuleData {
    pub fn validate_data(&self)->Result<()> {
        ensure!(self.version==1&&self.components.len()==MAX_RUNTIME_COMPONENTS&&self.selected_executables.len()==4,"runtime capsule shape differs");
        uuid(&self.request_nonce)?;uuid(&self.engine_instance_id)?;uuid(&self.boot_id)?;
        for sha in [&self.binding_sha256,&self.config_sha256,&self.registration_proof_sha256,&self.launcher_sha256,&self.service_sha256,&self.engine_sha256]{hash_text(sha)?;}
        ensure!(self.epoch.parse::<u64>().is_ok_and(|v|v>0&&v.to_string()==self.epoch)&&self.helper_start_ticks.parse::<u64>().is_ok_and(|v|v.to_string()==self.helper_start_ticks),"runtime epoch/start identity invalid");
        for pid in [self.launcher_pid,self.service_pid,self.engine_pid,self.helper_pid]{ensure!(pid>1,"runtime original PID invalid");}
        for reference in [&self.inventory,&self.manifest,&self.installer_input]{reference.validate()?;}
        ensure!(self.inventory.kind==crate::protocol::ObjectKind::Blob&&self.inventory.size_bytes<=16*1024*1024
            &&self.manifest.kind==crate::protocol::ObjectKind::Blob&&self.manifest.size_bytes<=32*1024*1024
            &&self.installer_input.kind==crate::protocol::ObjectKind::Directory,"runtime qualified roots differ");
        ensure!(self.floor.directory&&self.floor.mount_id>0,"runtime floor mount invalid");
        for (entry,(_,_,_,dir)) in self.components.iter().zip(COMPONENTS){ensure!(entry.directory==dir&&entry.mount_id==self.floor.mount_id,"runtime component role/mount differs");}
        let expected=["package/dist/native/rudder-native","package/dist/native/rudder-jj","usr/local/bin/rudder-root-storage","usr/local/bin/rudder-machine-root"];
        for (entry,path) in self.selected_executables.iter().zip(expected){hash_text(&entry.sha256)?;ensure!(entry.installer_path==path&&entry.size_bytes>0&&entry.size_bytes<=512*1024*1024,"runtime selected executable differs");}
        ensure!(self.selected_executables[0].sha256==self.engine_sha256&&self.selected_executables[2].sha256==self.service_sha256
            &&self.selected_executables[3].sha256==self.launcher_sha256,"runtime selected original images differ");Ok(())
    }
}
/// Kernel mechanics only. Capturing or sending on this anonymous endpoint
/// creates no runtime grant. Used by the original Root service and receiver.
pub struct RuntimeEndpoint {fd:OwnedFd}
pub struct RuntimePacket {pub bytes:Vec<u8>,pub fds:Vec<OwnedFd>,pub credentials:libc::ucred}
impl RuntimeEndpoint {
    pub fn capture(fd:OwnedFd)->Result<Self>{
        ensure!(unsafe{libc::fcntl(fd.as_raw_fd(),libc::F_SETFD,libc::FD_CLOEXEC)}==0,"runtime endpoint CLOEXEC unavailable");
        for (option,expected) in [(libc::SO_TYPE,libc::SOCK_SEQPACKET),(libc::SO_DOMAIN,libc::AF_UNIX)]{
            let mut actual=0i32;let mut n=std::mem::size_of::<i32>()as libc::socklen_t;
            ensure!(unsafe{libc::getsockopt(fd.as_raw_fd(),libc::SOL_SOCKET,option,(&mut actual as *mut i32).cast(),&mut n)}==0&&actual==expected,"runtime endpoint kind differs");}
        let mut name:libc::sockaddr_un=unsafe{std::mem::zeroed()};let mut n=std::mem::size_of_val(&name)as libc::socklen_t;
        ensure!(unsafe{libc::getpeername(fd.as_raw_fd(),(&mut name as *mut libc::sockaddr_un).cast(),&mut n)}==0
            &&(n as usize==std::mem::size_of::<libc::sa_family_t>()||name.sun_path[0]==0),"runtime endpoint is not original anonymous/abstract pair");
        let flags=unsafe{libc::fcntl(fd.as_raw_fd(),libc::F_GETFL)};
        ensure!(flags>=0&&flags&libc::O_NONBLOCK!=0,"runtime endpoint must be nonblocking");let yes=1i32;
        ensure!(unsafe{libc::setsockopt(fd.as_raw_fd(),libc::SOL_SOCKET,libc::SO_PASSCRED,(&yes as *const i32).cast(),std::mem::size_of_val(&yes)as _)}==0,"runtime credentials unavailable");Ok(Self{fd})
    }
    pub fn duplicate_transport_fd(&self)->Result<OwnedFd>{Ok(self.fd.try_clone()?)}
    pub fn send_data(&self,bytes:&[u8],fds:&[&OwnedFd])->Result<bool>{
        ensure!(!bytes.is_empty()&&bytes.len()<=MAX_FRAME&&fds.len()<=MAX_RUNTIME_FDS,"runtime packet bound exceeded");
        let mut iov=libc::iovec{iov_base:bytes.as_ptr().cast_mut().cast(),iov_len:bytes.len()};let mut control=[0usize;32];let mut msg:libc::msghdr=unsafe{std::mem::zeroed()};msg.msg_iov=&mut iov;msg.msg_iovlen=1;
        if !fds.is_empty(){msg.msg_control=control.as_mut_ptr().cast();msg.msg_controllen=unsafe{libc::CMSG_SPACE((fds.len()*4)as _)}as usize;
            unsafe{let h=libc::CMSG_FIRSTHDR(&msg);(*h).cmsg_level=libc::SOL_SOCKET;(*h).cmsg_type=libc::SCM_RIGHTS;(*h).cmsg_len=libc::CMSG_LEN((fds.len()*4)as _)as usize;
                for(i,fd)in fds.iter().enumerate(){std::ptr::write_unaligned(libc::CMSG_DATA(h).cast::<i32>().add(i),fd.as_raw_fd());}}}
        let n=unsafe{libc::sendmsg(self.fd.as_raw_fd(),&msg,libc::MSG_DONTWAIT|libc::MSG_NOSIGNAL)};
        if n<0&&io::Error::last_os_error().kind()==io::ErrorKind::WouldBlock{return Ok(false)}
        ensure!(n==bytes.len()as isize,"runtime send outcome unknown; retain original owners");Ok(true)
    }
    pub fn receive_data(&self)->Result<Option<RuntimePacket>>{
        let mut bytes=vec![0;MAX_FRAME];let mut control=[0usize;64];let mut iov=libc::iovec{iov_base:bytes.as_mut_ptr().cast(),iov_len:bytes.len()};let mut msg:libc::msghdr=unsafe{std::mem::zeroed()};
        msg.msg_iov=&mut iov;msg.msg_iovlen=1;msg.msg_control=control.as_mut_ptr().cast();msg.msg_controllen=std::mem::size_of_val(&control);
        let n=unsafe{libc::recvmsg(self.fd.as_raw_fd(),&mut msg,libc::MSG_DONTWAIT|libc::MSG_CMSG_CLOEXEC)};
        if n<0&&io::Error::last_os_error().kind()==io::ErrorKind::WouldBlock{return Ok(None)}
        let mut fds=Vec::new();let mut credentials=None;let mut valid=true;
        unsafe{let mut h=libc::CMSG_FIRSTHDR(&msg);while !h.is_null(){let base=libc::CMSG_LEN(0)as usize;if (*h).cmsg_len<base{valid=false;break}let size=(*h).cmsg_len-base;
            if (*h).cmsg_level==libc::SOL_SOCKET&&(*h).cmsg_type==libc::SCM_RIGHTS{if size%4!=0{valid=false}for i in 0..size/4{fds.push(OwnedFd::from_raw_fd(std::ptr::read_unaligned(libc::CMSG_DATA(h).cast::<i32>().add(i))))}}
            else if (*h).cmsg_level==libc::SOL_SOCKET&&(*h).cmsg_type==libc::SCM_CREDENTIALS&&size==std::mem::size_of::<libc::ucred>()&&credentials.is_none(){credentials=Some(std::ptr::read_unaligned(libc::CMSG_DATA(h).cast::<libc::ucred>()))}
            else{valid=false}h=libc::CMSG_NXTHDR(&msg,h);}}
        ensure!(n>0&&valid&&fds.len()<=MAX_RUNTIME_FDS&&msg.msg_flags&(libc::MSG_TRUNC|libc::MSG_CTRUNC)==0,"runtime packet closed/truncated/unknown; originals retained");
        bytes.truncate(n as usize);Ok(Some(RuntimePacket{bytes,fds,credentials:credentials.context("runtime actual SCM credentials absent")?}))
    }
}
pub fn runtime_socket_pair()->Result<(OwnedFd,OwnedFd)>{let mut raw=[-1;2];ensure!(unsafe{libc::socketpair(libc::AF_UNIX,libc::SOCK_SEQPACKET|libc::SOCK_NONBLOCK|libc::SOCK_CLOEXEC,0,raw.as_mut_ptr())}==0,"runtime anonymous pair unavailable");
    let first=unsafe{OwnedFd::from_raw_fd(raw[0])};let second=unsafe{OwnedFd::from_raw_fd(raw[1])};let yes=1i32;
    // Credentials must be enabled BEFORE a grant can be queued. Turning this
    // on only after a Root ACK/descriptor attachment loses original sender
    // credentials on an already queued packet.
    for fd in [&first,&second]{ensure!(unsafe{libc::setsockopt(fd.as_raw_fd(),libc::SOL_SOCKET,libc::SO_PASSCRED,(&yes as *const i32).cast(),std::mem::size_of_val(&yes)as _)}==0,"runtime pre-send credentials unavailable");}
    Ok((first,second))}

pub(crate) fn mount_id(fd:RawFd)->Result<u64>{let data=fs::read_to_string(format!("/proc/self/fdinfo/{fd}"))?;ensure!(data.len()<=4096,"runtime fdinfo exceeds bound");
    data.lines().find_map(|l|l.strip_prefix("mnt_id:\t")).context("runtime FD mount ID absent")?.parse().context("runtime mount ID invalid")}
pub fn fd_data(fd:&OwnedFd)->Result<RuntimeFdData>{let file=File::from(fd.try_clone()?);let m=file.metadata()?;ensure!(m.is_dir()||m.is_file(),"runtime component type invalid");
    Ok(RuntimeFdData{device:m.dev(),inode:m.ino(),mount_id:mount_id(fd.as_raw_fd())?,directory:m.is_dir()})}
fn pid_from_fd(fd:&OwnedFd)->Result<u32>{ensure!(fs::read_link(format!("/proc/self/fd/{}",fd.as_raw_fd()))?==Path::new("anon_inode:[pidfd]"),"runtime actual pidfd absent");
    let data=fs::read_to_string(format!("/proc/self/fdinfo/{}",fd.as_raw_fd()))?;ensure!(data.len()<=4096,"runtime pidfd info bound");let pid:u32=data.lines().find_map(|l|l.strip_prefix("Pid:\t")).context("runtime pidfd PID absent")?.parse()?;ensure!(pid>1,"runtime original process exited");live(fd)?;Ok(pid)}
fn live(fd:&OwnedFd)->Result<()>{let mut p=libc::pollfd{fd:fd.as_raw_fd(),events:libc::POLLIN,revents:0};ensure!(unsafe{libc::poll(&mut p,1,0)}==0,"runtime original process exited");Ok(())}
fn proc_start(pid:u32)->Result<String>{let data=fs::read_to_string(format!("/proc/{pid}/stat"))?;ensure!(data.len()<=4096,"runtime process stat bound");let(_,fields)=data.rsplit_once(')').context("runtime process stat incomplete")?;
    Ok(fields.split_whitespace().nth(19).context("runtime process start absent")?.to_owned())}
fn parent_pid(pid:u32)->Result<u32>{let data=fs::read_to_string(format!("/proc/{pid}/status"))?;ensure!(data.len()<=16384,"runtime process status bound");data.lines().find_map(|l|l.strip_prefix("PPid:")).context("runtime process parent absent")?.trim().parse().context("runtime parent invalid")}
fn floor(pid:u32,uid:u32)->Result<()>{let status=fs::read_to_string(format!("/proc/{pid}/status"))?;ensure!(status.len()<=16384,"runtime process status bound");
    for key in ["Uid:","Gid:"]{let values=status.lines().find_map(|l|l.strip_prefix(key)).context("runtime process IDs absent")?;ensure!(values.split_whitespace().count()==4&&values.split_whitespace().all(|v|v==uid.to_string()),"runtime process IDs differ");}
    ensure!(status.lines().any(|l|l.strip_prefix("TracerPid:").is_some_and(|v|v.trim()=="0")),"runtime process traced");
    if uid==1500{for key in ["CapEff:","CapPrm:","CapBnd:","CapInh:","CapAmb:"]{ensure!(status.lines().find_map(|l|l.strip_prefix(key)).is_some_and(|v|v.trim()=="0000000000000000"),"runtime helper capabilities differ");}
        ensure!(status.lines().any(|l|l.strip_prefix("NoNewPrivs:").is_some_and(|v|v.trim()=="1"))&&status.lines().any(|l|l.strip_prefix("Groups:").is_some_and(|v|v.trim().is_empty())),"runtime helper privilege floor differs");}Ok(())}
fn same_file(a:&File,b:&File)->Result<bool>{let a=a.metadata()?;let b=b.metadata()?;Ok((a.dev(),a.ino(),a.len(),a.ctime(),a.ctime_nsec())==(b.dev(),b.ino(),b.len(),b.ctime(),b.ctime_nsec()))}
fn hash_file(file:&File,max:u64,original:&FinalizationBudget)->Result<String>{let m=file.metadata()?;ensure!(m.is_file()&&m.len()>0&&m.len()<=max,"runtime executable bound invalid");let mut hash=Sha256::new();let mut data=[0;65536];let mut at=0;
    while at<m.len(){original.remaining()?;let n=file.read_at(&mut data[..usize::try_from((m.len()-at).min(65536))?],at)?;ensure!(n>0,"runtime executable read incomplete");hash.update(&data[..n]);at+=n as u64;}
    let after=file.metadata()?;original.remaining()?;ensure!((after.dev(),after.ino(),after.len(),after.mode(),after.ctime(),after.ctime_nsec())==(m.dev(),m.ino(),m.len(),m.mode(),m.ctime(),m.ctime_nsec()),"runtime executable changed");Ok(hex::encode(hash.finalize()))}
fn verify_image(_pid:u32,fd:&OwnedFd,path:&str,hash:&str,original:&FinalizationBudget)->Result<()>{
    for parent in Path::new(path).parent().unwrap().ancestors(){let m=fs::symlink_metadata(parent)?;ensure!(m.is_dir()&&!m.file_type().is_symlink()&&m.uid()==0&&m.gid()==0&&m.mode()&0o022==0,"runtime original image ancestry differs");}
    let file=File::from(fd.try_clone()?);let m=file.metadata()?;ensure!(m.is_file()&&m.uid()==0&&m.gid()==0&&m.nlink()==1&&m.mode()&0o022==0,"runtime image is not immutable global Root");
    // The original Root producer bound proc-exe to the original pidfd before
    // transfer. Reopening a non-dumpable cross-UID /proc/<pid>/exe here is both
    // unavailable and would replace that original witness with a pathname.
    let named=OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW|libc::O_CLOEXEC).open(path)?;
    ensure!(same_file(&file,&named)?&&hash_file(&file,512*1024*1024,original)?==hash,"runtime original executable differs");Ok(())}
fn initial_global_namespace(fd:&OwnedFd,name:&str,kind:i32,pids:&[u32])->Result<()>{let held=File::from(fd.try_clone()?);let m=held.metadata()?;
    ensure!(unsafe{libc::ioctl(fd.as_raw_fd(),libc::NS_GET_NSTYPE as libc::c_ulong)}==kind,"runtime original namespace type differs");
    // Root verified the entire original chain while it still held SYS_PTRACE.
    // The unprivileged helper must consume those original namespace FDs,
    // rather than reopening protected cross-UID /proc namespace symlinks.
    let _=pids;for pid in [std::process::id()]{let n=File::open(format!("/proc/{pid}/ns/{name}"))?.metadata()?;ensure!((n.dev(),n.ino())==(m.dev(),m.ino()),"runtime namespace is not original global namespace");}
    Ok(())}
/// Both uid_map and gid_map are read from actual held original Root processes.
/// Namespace-local uid0/PID1 cannot substitute the initial identity mapping.
pub fn require_initial_id_maps(pid:u32)->Result<()>{for name in ["uid_map","gid_map"]{let data=fs::read_to_string(format!("/proc/{pid}/{name}"))?;ensure!(data.len()<=4096,"runtime ID map bound");let lines:Vec<_>=data.lines().collect();ensure!(lines.len()==1&&lines[0].split_whitespace().eq(["0","0","4294967295"]),"runtime Root is not initial USER namespace");}Ok(())}
fn sealed_root_bytes(fd:&OwnedFd,global:bool)->Result<Vec<u8>>{let file=File::from(fd.try_clone()?);let m=file.metadata()?;let flags=unsafe{libc::fcntl(fd.as_raw_fd(),libc::F_GETFL)};let seals=unsafe{libc::fcntl(fd.as_raw_fd(),libc::F_GET_SEALS)};
    ensure!(m.is_file()&&m.nlink()==0&&m.len()>0&&m.len()<=MAX_FRAME as u64&&flags>=0&&flags&libc::O_ACCMODE==libc::O_RDONLY&&seals>=0&&seals&SEALED==SEALED,"runtime original Root sealed capsule absent");
    if global{ensure!(m.uid()==0&&m.gid()==0,"runtime capsule was not issued by global Root");}
    let mut bytes=vec![0;m.len()as usize];let mut at=0;while at<bytes.len(){let n=file.read_at(&mut bytes[at..],at as u64)?;ensure!(n>0,"runtime capsule read incomplete");at+=n;}Ok(bytes)}
/// Endpoint transport only. It grants no mount, registration or execution.
/// The helper still must authenticate the live original service packet.
pub struct RuntimeImageHandoffAttachment {endpoint:OwnedFd}
impl RuntimeImageHandoffAttachment {pub fn from_original_endpoint(endpoint:OwnedFd)->Result<Self>{let endpoint=RuntimeEndpoint::capture(endpoint)?;Ok(Self{endpoint:endpoint.fd})}
    pub(crate) fn descriptor(&self)->RawFd{self.endpoint.as_raw_fd()}}
/// Actual service capsule + original process/image/namespace and component FDs.
/// No Clone/Serde/public constructor. No cleanup is attempted by Drop.
pub struct RuntimeImageGrant {data:RuntimeImageCapsuleData,fds:Vec<OwnedFd>,closed:AtomicBool,validated:bool}
impl RuntimeImageGrant {
    pub(crate) fn receive_retaining(out:&mut Option<Self>,pending:&mut Option<RuntimePacket>,endpoint:&RuntimeEndpoint,binding:&super::ManagedExecutionBinding)->Result<bool>{
        ensure!(out.is_none()&&pending.is_none(),"runtime original receive slot occupied");let Some(packet)=endpoint.receive_data()?else{return Ok(false)};*pending=Some(packet);
        let p=pending.as_ref().unwrap();let data:RuntimeImageCapsuleData=serde_json::from_slice(&p.bytes)?;data.validate_data()?;
        ensure!(p.fds.len()==MAX_RUNTIME_FDS&&p.credentials.pid==data.service_pid as i32&&p.credentials.uid==0&&p.credentials.gid==0,"runtime capsule is not original Root service packet");
        ensure!(data.helper_pid==std::process::id()&&data.engine_pid==unsafe{libc::getppid()}as u32&&data.binding_sha256==hex::encode(Sha256::digest(canonical_json(binding)?)),"runtime capsule belongs to another original helper/assignment");
        let original=FinalizationBudget::from_constraint(&data.constraint)?;original.remaining()?;
        ensure!(sealed_root_bytes(&p.fds[0],true)?==p.bytes,"runtime live packet differs from original sealed Root capsule");
        for(index,pid,uid)in [(1,data.launcher_pid,0),(3,data.service_pid,0),(5,data.engine_pid,1500),(7,data.helper_pid,1500)]{ensure!(pid_from_fd(&p.fds[index])?==pid,"runtime original pidfd differs");floor(pid,uid)?;}
        ensure!(parent_pid(data.service_pid)?==data.launcher_pid&&parent_pid(data.engine_pid)?==data.launcher_pid&&parent_pid(data.helper_pid)?==data.engine_pid
            &&proc_start(data.helper_pid)?==data.helper_start_ticks,"runtime original process tree/start changed");
        require_initial_id_maps(data.launcher_pid)?;require_initial_id_maps(data.service_pid)?;require_initial_id_maps(data.engine_pid)?;require_initial_id_maps(data.helper_pid)?;
        verify_image(data.launcher_pid,&p.fds[2],LAUNCHER_IMAGE,&data.launcher_sha256,&original)?;verify_image(data.service_pid,&p.fds[4],SERVICE_IMAGE,&data.service_sha256,&original)?;
        verify_image(data.engine_pid,&p.fds[6],ENGINE_IMAGE,&data.engine_sha256,&original)?;
        // Helper is the SAME pinned native image; full hash was just checked on
        // the original engine proc-exe FD, and its own proc-exe must match it.
        ensure!(same_file(&File::from(p.fds[6].try_clone()?),&File::open("/proc/self/exe")?)?,"runtime helper native image differs");
        for(index,name,kind)in [(8,"user",libc::CLONE_NEWUSER),(9,"mnt",libc::CLONE_NEWNS),(10,"pid",libc::CLONE_NEWPID)]{
            initial_global_namespace(&p.fds[index],name,kind,&[data.launcher_pid,data.service_pid,data.engine_pid,data.helper_pid])?;}
        ensure!(fs::read_to_string("/proc/sys/kernel/random/boot_id")?.trim()==data.boot_id,"runtime original boot differs");
        // The protected public config (not token/key) is an actual original FD.
        let config=File::from(p.fds[11].try_clone()?);let cm=config.metadata()?;ensure!(cm.uid()==0&&cm.gid()==0&&cm.mode()&0o7777==0o600&&cm.nlink()==1&&cm.len()<=32*1024&&unsafe{libc::fcntl(p.fds[11].as_raw_fd(),libc::F_GETFL)}&libc::O_ACCMODE==libc::O_RDONLY
            &&hash_file(&config,32*1024,&original)?==data.config_sha256,"runtime original protected config differs");
        let fds=pending.take().unwrap().fds;*out=Some(Self{data,fds,closed:AtomicBool::new(false),validated:false});
        let result=(||->Result<()>{let value=out.as_ref().unwrap();value.check_component_bindings()?;FinalizationBudget::from_constraint(&value.data.constraint)?.remaining()?;Ok(())})();
        if let Err(error)=result{out.as_ref().unwrap().close_admission();return Err(error)}out.as_mut().unwrap().validated=true;Ok(true)
    }
    fn check_components(&self)->Result<()>{ensure!(self.validated,"runtime original complete grant ACK absent");self.check_component_bindings()}
    fn check_component_bindings(&self)->Result<()>{ensure!(!self.closed.load(Ordering::Acquire)&&self.fds.len()==MAX_RUNTIME_FDS,"runtime grant closed/incomplete");
        for i in [1,3,5,7]{live(&self.fds[i])?;}
        let mut cancel=libc::pollfd{fd:self.fds[13].as_raw_fd(),events:libc::POLLIN,revents:0};
        ensure!(fs::read_link(format!("/proc/self/fd/{}",self.fds[13].as_raw_fd()))?==Path::new("anon_inode:[eventfd]")
            &&unsafe{libc::poll(&mut cancel,1,0)}==0,"runtime original helper request cancelled");ensure!(fd_data(&self.fds[12])?==self.data.floor,"runtime held floor FD changed");
        let mut stat:libc::statfs=unsafe{std::mem::zeroed()};ensure!(unsafe{libc::fstatfs(self.fds[12].as_raw_fd(),&mut stat)}==0&&stat.f_type==0x65735546,"runtime floor is not original FUSE");
        for(i,expected)in self.data.components.iter().enumerate(){let fd=&self.fds[RUNTIME_FIXED_FDS+i];ensure!(fd_data(fd)?==*expected,"runtime held component inode/mount changed");let mut v:libc::statvfs=unsafe{std::mem::zeroed()};
            ensure!(unsafe{libc::fstatvfs(fd.as_raw_fd(),&mut v)}==0&&v.f_flag&((libc::ST_RDONLY|libc::ST_NOSUID|libc::ST_NODEV)as libc::c_ulong)==((libc::ST_RDONLY|libc::ST_NOSUID|libc::ST_NODEV)as libc::c_ulong)
                &&v.f_flag&libc::ST_NOEXEC as libc::c_ulong==0,"runtime component mount permissions differ");}Ok(())}
    pub fn close_admission(&self){self.closed.store(true,Ordering::Release)}
    pub(crate) fn component(&self,runtime:&ReadOnlyRuntime)->Result<Option<(&OwnedFd,bool)>>{
        self.check_components()?;if let Some((i,(_,source,_,dir)))=COMPONENTS.iter().enumerate().find(|(_,(_,_,guest,_))|*guest==runtime.guest_path){
            ensure!(runtime.source==Path::new(source),"runtime projection differs from exact approved source DATA");return Ok(Some((&self.fds[RUNTIME_FIXED_FDS+i],*dir)))}
        ensure!(["/etc/resolv.conf","/etc/hosts","/etc/nsswitch.conf"].contains(&runtime.guest_path.as_str())&&runtime.source==Path::new(&runtime.guest_path),"runtime image grant refuses ordinary image-path fallback/remap");Ok(None)
    }
    pub(crate) fn assert_spec(&self,spec:&ExecutionSpec)->Result<()>{self.check_components()?;
        for(_,source,guest,_)in COMPONENTS{ensure!(spec.runtime.iter().filter(|r|r.guest_path==guest&&r.source==Path::new(source)).count()==1,"runtime spec lacks exact immutable component projection");}
        for runtime in &spec.runtime{self.component(runtime)?;}Ok(())
    }
    pub(crate) fn verify_provider_projection(&self,root:&Path,kernel:bool)->Result<()> {
        self.check_components()?;let relative=if kernel{"lib/modules"}else{"sbin/swapon"};
        let original_path=format!("/proc/self/fd/{}/{}",self.fds[RUNTIME_FIXED_FDS].as_raw_fd(),relative);
        let original=OpenOptions::new().read(true).custom_flags(libc::O_PATH|libc::O_NOFOLLOW|libc::O_CLOEXEC).open(original_path)?;
        let guest=root.join(if kernel{"usr/lib/modules"}else{"usr/sbin/swapon"});
        let named=OpenOptions::new().read(true).custom_flags(libc::O_PATH|libc::O_NOFOLLOW|libc::O_CLOEXEC).open(&guest)?;
        let a=original.metadata()?;let b=named.metadata()?;
        ensure!((a.dev(),a.ino(),a.mode())==(b.dev(),b.ino(),b.mode()),"runtime provider projection replaced after original held component bind");
        ensure!(mount_id(original.as_raw_fd())?==self.data.floor.mount_id,"runtime provider path escaped exact FUSE floor");
        if kernel {ensure!(a.is_dir()&&a.mode()&0o7777==0o555&&fs::read_dir(&guest)?.next().is_none(),"runtime provider modules are not retained empty directory");}
        else{ensure!(a.is_file()&&a.len()==0&&a.mode()&0o7777==0o444,"runtime provider swapon is not retained empty file");}Ok(())
    }
    pub(crate) fn duplicate_for_exec(&self)->Result<Vec<OwnedFd>>{self.check_components()?;self.fds.iter().map(|fd|fd.try_clone().map_err(Into::into)).collect()}
    pub(crate) fn data_for_exec(&self)->RuntimeImageCapsuleData{self.data.clone()}
    /// Only execution's private sealed ChildSpec path can call this. No live
    /// SCM proof is recreated: it moves the SAME exact fd allowlist from the
    /// already authenticated fresh helper, with unchanged sealed Root bytes.
    /// The wrapper retains the original helper pidfd and checks actual parent.
    fn after_original_helper_exec_retaining(out:&mut Option<Self>,data:RuntimeImageCapsuleData,fds:Vec<OwnedFd>)->Result<()>{
        ensure!(out.is_none(),"runtime post-exec owner occupied");*out=Some(Self{data,fds,closed:AtomicBool::new(false),validated:false});
        let result=out.as_ref().unwrap().validate_original_helper_exec();
        if let Err(error)=result{out.as_ref().unwrap().close_admission();return Err(error)}out.as_mut().unwrap().validated=true;Ok(())
    }
    fn validate_original_helper_exec(&self)->Result<()>{let data=&self.data;let fds=&self.fds;
        data.validate_data()?;for fd in fds{ensure!(unsafe{libc::fcntl(fd.as_raw_fd(),libc::F_SETFD,libc::FD_CLOEXEC)}==0,"runtime wrapper descriptor protection unavailable");}ensure!(fds.len()==MAX_RUNTIME_FDS&&data.helper_pid==unsafe{libc::getppid()}as u32&&pid_from_fd(&fds[7])?==data.helper_pid
            &&proc_start(data.helper_pid)?==data.helper_start_ticks,"runtime wrapper changed original helper parent");
        // Child is already in the helper's one-user namespace. The original
        // Root memfd is unmapped, while helper-created files map to uid1500. This
        // is NOT a public unmapped-UID issuer: only this private original-exec
        // path consumes the fixed capsule already authenticated before unshare.
        for name in ["uid_map","gid_map"]{let map=fs::read_to_string(format!("/proc/self/{name}"))?;ensure!(map.split_whitespace().eq(["1500","1500","1"]),"runtime wrapper did not inherit exact original helper USER namespace");}
        let m=File::from(fds[0].try_clone()?).metadata()?;ensure!(m.uid()==65534&&m.gid()==65534,"runtime wrapper lost original global Root sealed memfd");
        ensure!(sealed_root_bytes(&fds[0],false)?==canonical_json(&data)?,"runtime wrapper sealed Root capsule changed");
        self.check_component_bindings()?;FinalizationBudget::from_constraint(&data.constraint)?.remaining()?;Ok(())
    }
}
#[cfg(test)]
#[path="runtime_image_tests.rs"]
mod tests;

/// Policy0 keeps credential/user-namespace transitions nondumpable throughout
/// their interval, as in the machine's qualified Root entry. Checking ND0 only
/// AFTER the transition would not establish absence of a descriptor window.
pub(crate) struct RuntimeNamespaceGuard {file:File,id:(u64,u64)}
impl RuntimeNamespaceGuard {
    pub(crate) fn capture()->Result<Self>{ensure!(unsafe{libc::prctl(libc::PR_GET_DUMPABLE,0,0,0,0)}==0,"runtime namespace transition requires ND0");
        let file=OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW|libc::O_CLOEXEC|libc::O_NONBLOCK).open("/proc/sys/fs/suid_dumpable")?;let m=file.metadata()?;
        let value=Self{file,id:(m.dev(),m.ino())};value.check()?;Ok(value)}
    pub(crate) fn check(&self)->Result<()>{let m=self.file.metadata()?;let named=fs::symlink_metadata("/proc/sys/fs/suid_dumpable")?;
        let mut kind:libc::statfs=unsafe{std::mem::zeroed()};let mut b=[0;8];let n=self.file.read_at(&mut b,0)?;
        ensure!(unsafe{libc::fstatfs(self.file.as_raw_fd(),&mut kind)}==0&&kind.f_type==0x9fa0&&(m.dev(),m.ino())==self.id&&(named.dev(),named.ino())==self.id
            &&m.mode()&0o022==0&&n==2&&&b[..n]==b"0\n"&&unsafe{libc::prctl(libc::PR_GET_DUMPABLE,0,0,0,0)}==0,"runtime retained policy0/ND0 transition floor changed");Ok(())}
}
fn peer_credentials(fd:RawFd)->Result<libc::ucred>{let mut value:libc::ucred=unsafe{std::mem::zeroed()};let mut len=std::mem::size_of_val(&value)as libc::socklen_t;
    ensure!(unsafe{libc::getsockopt(fd,libc::SOL_SOCKET,libc::SO_PEERCRED,(&mut value as *mut libc::ucred).cast(),&mut len)}==0&&len as usize==std::mem::size_of_val(&value),"runtime original exec peer absent");Ok(value)}
fn original_peer_pidfd(fd:RawFd)->Result<OwnedFd>{
    // Linux UAPI SO_PEERPIDFD, available since6.5. No unsafe compatibility
    // fallback: a preconnected dead peer's numeric PID can be reused.
    let mut raw=-1i32;let mut len=std::mem::size_of::<i32>()as libc::socklen_t;
    ensure!(unsafe{libc::getsockopt(fd,libc::SOL_SOCKET,77,(&mut raw as *mut i32).cast(),&mut len)}==0&&raw>=0,"runtime kernel SO_PEERPIDFD unavailable");
    let held=unsafe{OwnedFd::from_raw_fd(raw)};ensure!(len as usize==std::mem::size_of::<i32>(),"runtime kernel peer pidfd shape differs");ensure!(unsafe{libc::fcntl(held.as_raw_fd(),libc::F_SETFD,libc::FD_CLOEXEC)}==0,"runtime peer pidfd CLOEXEC failed");Ok(held)
}
fn abstract_address(name:&str)->Result<(libc::sockaddr_un,libc::socklen_t)>{ensure!(name.starts_with("rudder-runtime-exec-")&&name.len()<=80&&name.bytes().all(|b|b.is_ascii_alphanumeric()||b==b'-'),"runtime exec routing invalid");
    let mut address:libc::sockaddr_un=unsafe{std::mem::zeroed()};address.sun_family=libc::AF_UNIX as _;for(i,b)in name.bytes().enumerate(){address.sun_path[1+i]=b as _;}
    Ok((address,(std::mem::size_of::<libc::sa_family_t>()+1+name.len())as _))}
/// One original wrapper process only. No authority FD or prequeued grant is
/// inherited through exec's dumpable reset. The protected wrapper creates a
/// NEW connection after ND0, authenticated by original child pidfd/SO_PEERCRED.
/// Source originals and all transfer copies remain owned after unknown sends.
pub(crate) struct RuntimeExecHandoff {listener:Option<OwnedFd>,name:String,child:Option<OwnedFd>,connection:Option<RuntimeEndpoint>,peer:Option<OwnedFd>,fds:Vec<OwnedFd>,bytes:Vec<u8>,original:FinalizationBudget,sent:bool,unknown:bool}
impl RuntimeExecHandoff {
    pub(crate) fn capture_retaining(out:&mut Option<Self>,grant:&RuntimeImageGrant)->Result<()>{ensure!(out.is_none(),"runtime original exec handoff occupied");
        let original=FinalizationBudget::from_constraint(&grant.data.constraint)?;original.remaining()?;
        Self::capture_transport_retaining(out,original,canonical_json(&grant.data)?,grant.duplicate_for_exec()?)
    }
    // Kernel transport only; private so production enters exclusively through
    // the actual already authenticated grant above. Component tests can use
    // harmless real FDs without fabricating any Root grant/readiness owner.
    fn capture_transport_retaining(out:&mut Option<Self>,original:FinalizationBudget,bytes:Vec<u8>,fds:Vec<OwnedFd>)->Result<()>{
        ensure!(out.is_none()&&!bytes.is_empty()&&bytes.len()<=MAX_FRAME&&fds.len()==MAX_RUNTIME_FDS,"runtime transport original bounds differ");original.remaining()?;
        *out=Some(Self{listener:None,name:format!("rudder-runtime-exec-{}",Uuid::new_v4()),child:None,connection:None,peer:None,fds,bytes,original,sent:false,unknown:false});
        let owner=out.as_mut().unwrap();
        let raw=unsafe{libc::socket(libc::AF_UNIX,libc::SOCK_SEQPACKET|libc::SOCK_CLOEXEC|libc::SOCK_NONBLOCK,0)};ensure!(raw>=0,"runtime exec listener unavailable");owner.listener=Some(unsafe{OwnedFd::from_raw_fd(raw)});
        let(address,len)=abstract_address(&owner.name)?;ensure!(unsafe{libc::bind(raw,(&address as *const libc::sockaddr_un).cast(),len)}==0&&unsafe{libc::listen(raw,4)}==0,"runtime original exec listener refused");owner.original.remaining()?;Ok(())
    }
    pub(crate) fn name_data(&self)->&str{&self.name}
    pub(crate) fn dispatch_original_child_retaining(&mut self,child:OwnedFd)->Result<()>{ensure!(self.child.is_none()&&!self.sent&&!self.unknown,"runtime original wrapper dispatch cannot replay");self.child=Some(child);
        let pid=pid_from_fd(self.child.as_ref().unwrap())?;let mut rejected=0;
        loop{self.original.remaining()?;live(self.child.as_ref().unwrap())?;
            if self.connection.is_none(){let fd=unsafe{libc::accept4(self.listener.as_ref().unwrap().as_raw_fd(),std::ptr::null_mut(),std::ptr::null_mut(),libc::SOCK_CLOEXEC|libc::SOCK_NONBLOCK)};
                if fd<0{let e=io::Error::last_os_error();if matches!(e.kind(),io::ErrorKind::WouldBlock|io::ErrorKind::Interrupted){std::thread::sleep(self.original.phase_limit(Duration::from_millis(2))?);continue}return Err(e.into())}
                let fd=unsafe{OwnedFd::from_raw_fd(fd)};let peer=peer_credentials(fd.as_raw_fd())?;
                let actual=original_peer_pidfd(fd.as_raw_fd())?;
                if peer.pid!=pid as i32||peer.uid!=1500||peer.gid!=1500||pid_from_fd(&actual).ok()!=Some(pid){rejected+=1;ensure!(rejected<=32,"runtime exec foreign peer bound exceeded");continue}
                // Connection was created only after the fixed wrapper's first
                // ND0 action. It cannot be stolen from its earlier exec window.
                self.peer=Some(actual);self.connection=Some(RuntimeEndpoint::capture(fd)?);
            }
            ensure!(pid_from_fd(self.peer.as_ref().unwrap())?==pid,"runtime original connected wrapper exited");
            let fds:Vec<_>=self.fds.iter().collect();match self.connection.as_ref().unwrap().send_data(&self.bytes,&fds){
                Ok(false)=>std::thread::sleep(self.original.phase_limit(Duration::from_millis(2))?),
                Ok(true)=>{self.sent=true;self.original.remaining()?;return Ok(())},
                Err(e)=>{self.unknown=true;return Err(e)}}
        }
    }
}
pub(crate) struct RuntimeExecReceive {endpoint:RuntimeEndpoint,peer:OwnedFd,pending:Option<RuntimePacket>,original:FinalizationBudget,data:RuntimeImageCapsuleData}
impl RuntimeExecReceive {
    pub(crate) fn connect_retaining(out:&mut Option<Self>,name:&str,data:RuntimeImageCapsuleData)->Result<()> {
        ensure!(out.is_none()&&unsafe{libc::prctl(libc::PR_GET_DUMPABLE,0,0,0,0)}==0,"runtime exec connect requires protected fresh wrapper");
        ensure!(data.helper_pid==unsafe{libc::getppid()}as u32,"runtime exec changed original helper parent");
        let original=FinalizationBudget::from_constraint(&data.constraint)?;original.remaining()?;
        let raw=unsafe{libc::socket(libc::AF_UNIX,libc::SOCK_SEQPACKET|libc::SOCK_CLOEXEC|libc::SOCK_NONBLOCK,0)};ensure!(raw>=0,"runtime protected exec socket unavailable");let fd=unsafe{OwnedFd::from_raw_fd(raw)};
        let yes=1i32;ensure!(unsafe{libc::setsockopt(fd.as_raw_fd(),libc::SOL_SOCKET,libc::SO_PASSCRED,(&yes as *const i32).cast(),std::mem::size_of_val(&yes)as _)}==0,"runtime protected receiver credentials unavailable");
        let(address,len)=abstract_address(name)?;
        // AF_UNIX bounded queue refusal has no connection effect and no grant
        // bytes. Refuse rather than start a second connection or new clock.
        ensure!(unsafe{libc::connect(fd.as_raw_fd(),(&address as *const libc::sockaddr_un).cast(),len)}==0,"runtime original exec connection refused");
        let peer=peer_credentials(fd.as_raw_fd())?;let actual=original_peer_pidfd(fd.as_raw_fd())?;
        ensure!(peer.pid==data.helper_pid as i32&&peer.uid==1500&&peer.gid==1500&&pid_from_fd(&actual)?==data.helper_pid,"runtime exec server is not original helper");
        *out=Some(Self{endpoint:RuntimeEndpoint::capture(fd)?,peer:actual,pending:None,original,data});out.as_ref().unwrap().original.remaining()?;Ok(())
    }
    pub(crate) fn receive_retaining(&mut self,out:&mut Option<RuntimeImageGrant>)->Result<bool>{ensure!(out.is_none(),"runtime wrapper grant slot occupied");self.original.remaining()?;
        ensure!(pid_from_fd(&self.peer)?==self.data.helper_pid,"runtime original connected helper exited");
        if self.pending.is_none(){self.pending=self.endpoint.receive_data()?;}let Some(packet)=&self.pending else{return Ok(false)};
        ensure!(packet.credentials.pid==self.data.helper_pid as i32&&packet.credentials.uid==1500&&packet.credentials.gid==1500
            &&packet.fds.len()==MAX_RUNTIME_FDS&&packet.bytes==canonical_json(&self.data)?,"runtime original protected helper transfer differs");
        self.original.remaining()?;let fds=self.pending.take().unwrap().fds;
        RuntimeImageGrant::after_original_helper_exec_retaining(out,self.data.clone(),fds)?;
        if let Err(error)=self.original.remaining(){out.as_ref().unwrap().close_admission();return Err(error)}Ok(true)
    }
}
