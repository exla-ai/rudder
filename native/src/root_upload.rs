//! Actual one-shot child custody. Nested under host_ingress; no public factory.
use super::*;
use super::super::engine_registration::engine_operations::OperationBudget;
use std::{io::Read, os::{unix::net::UnixStream}, process::{ChildStdout,ChildStderr,Stdio}};

const UPLOADER:&str="/opt/rudder-host-control/root-upload.mjs";
// DATA checks follow actual retained Root/config checks at the sole caller.
fn validate_upload_config(data:&HostIngressConfig,scope:&UploadScope)->Result<()> {
    data.validate()?;ensure!(scope.home_id==data.home_id&&data.projects.iter().any(|p|p.alias==scope.workspace_id),
        "upload original config home/project differs");cloud_control_origin(&data.cloud_origin)?;Ok(())
}
fn check_original_config(config:&ImmutableFile,digest:&str)->Result<()> {
    config.check()?;sha(digest)?;
    let parent=fs::symlink_metadata(config.path.parent().context("upload config parent absent")?)?;
    ensure!(parent.is_dir()&&parent.uid()==0&&parent.gid()==0&&parent.mode()&0o7777==0o700,"upload protected config directory changed");
    let flags=unsafe {libc::fcntl(config.file.as_raw_fd(),libc::F_GETFL)};
    ensure!(flags>=0&&flags&libc::O_ACCMODE==libc::O_RDONLY,"upload config must be original readonly descriptor");
    ensure!(format!("{:x}",Sha256::digest(read_at(&config.file,MAX_HOST_FRAME)?))==digest,"upload original config bytes changed");
    config.check()?;Ok(())
}
fn duplicate_original_config(config:&ImmutableFile,digest:&str)->Result<OwnedFd> {
    check_original_config(config,digest)?;let copy=config.file.try_clone()?;
    config.stamp.check(&copy,0o600)?;let flags=unsafe {libc::fcntl(copy.as_raw_fd(),libc::F_GETFL)};
    ensure!(flags>=0&&flags&libc::O_ACCMODE==libc::O_RDONLY,"upload config duplicate is not readonly");
    check_original_config(config,digest)?;Ok(copy.into())
}
#[derive(Clone,Debug,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UploadScope {
    pub version:u32,pub home_id:String,pub workspace_id:String,pub transfer_id:String,pub request_nonce:String,
    pub mode:String,pub maximum_objects:u32,pub maximum_bytes:u64,pub maximum_object_bytes:u64,
    pub constraint:super::super::engine_registration::engine_operations::OperationConstraint,
    pub manifest:Option<rudder_engine::protocol::ObjectRef>,pub maximum_manifest_bytes:Option<u64>,
}
impl UploadScope {pub(crate) fn validate(&self)->Result<()> {
    key(&self.home_id)?;key(&self.workspace_id)?;uuid(&self.transfer_id)?;uuid(&self.request_nonce)?;
    ensure!(self.version==(if self.mode=="image_batch_v1"{2}else{1}) && match self.mode.as_str(){"fresh"=>self.maximum_objects==128&&self.maximum_bytes==4*1024*1024&&self.maximum_object_bytes==1024*1024,
        "image"|"image_batch_v1"=>self.maximum_objects==100000&&self.maximum_bytes==2*1024*1024*1024&&self.maximum_object_bytes==4*1024*1024,_=>false},"upload scope invalid");
    if self.mode=="image"||self.mode=="image_batch_v1" {let r=self.manifest.as_ref().context("upload exact image manifest absent")?;r.validate()?;
        ensure!(r.kind==rudder_engine::protocol::ObjectKind::Blob&&self.maximum_manifest_bytes==Some(33554432)&&r.size_bytes<=33554432,"upload manifest exception differs");}
    else {ensure!(self.manifest.is_none()&&self.maximum_manifest_bytes.is_none(),"fresh upload cannot use image manifest exception");}Ok(())}}
/// The child is retained in Root's original operation bridge through actual
/// wait + BOTH piped EOFs. No Drop teardown, automatic effect replay or path
/// recovery. The native receives only native_stream, never token/config FDs.
pub(crate) struct RootUploader {
    scope:UploadScope,budget:OperationBudget,config:Option<ImmutableFile>,config_sha:String,token:Option<File>,token_stamp:Option<Stamp>,node:Option<Image>,
    script:Option<ImmutableFile>,uptime:Option<File>,boot:Vec<u8>,child_endpoint:Option<UnixStream>,native_stream:Option<OwnedFd>,
    child:Option<super::super::ChildOwner>,exe:Option<OwnedFd>,namespaces:Vec<(String,File,i32)>,
    stdout:Option<ChildStdout>,stderr:Option<ChildStderr>,out:Vec<u8>,err:Vec<u8>,out_eof:bool,err_eof:bool,
    attempted:bool,delivered:bool,kill_attempted:bool,failed:bool,
}
impl RootUploader {
    pub(crate) fn prepare_retaining(out:&mut Option<Self>,host:&RootHostControl,scope:UploadScope,budget:OperationBudget)->Result<()> {
        ensure!(out.is_none(),"upload child carrier occupied");
        *out=Some(Self {scope,budget,config:None,config_sha:String::new(),token:None,token_stamp:None,node:None,script:None,uptime:None,boot:Vec::new(),child_endpoint:None,native_stream:None,
            child:None,exe:None,namespaces:Vec::new(),stdout:None,stderr:None,out:Vec::new(),err:Vec::new(),out_eof:false,err_eof:false,
            attempted:false,delivered:false,kill_attempted:false,failed:false});
        let s=out.as_mut().unwrap();s.scope.validate()?;s.budget.remaining()?;host.current()?;
        // Stage SAME actual held file before token capture/spawn. No reopen by
        // path, caller origin, reconstructed config or new authority owner.
        s.config=Some(ImmutableFile {file:host.file.try_clone()?,stamp:host.stamp.clone(),path:HOST_CONFIG_PATH.into(),mode:0o600});
        s.config_sha=host.digest.clone();validate_upload_config(&host.data,&s.scope)?;
        check_original_config(s.config.as_ref().unwrap(),&s.config_sha)?;host.current()?;
        s.token=Some(host.token.try_clone()?);s.token_stamp=Some(host.token_stamp.clone());
        s.node=Some(Image::capture(Path::new(NODE))?);ensure!(s.node.as_ref().unwrap().sha256==host.node.sha256,"upload Node differs from original image");
        let original=host.images.iter().find(|i|i.path==Path::new(UPLOADER)).context("upload script absent from complete immutable sidecar closure")?;original.check()?;
        s.script=Some(ImmutableFile {file:original.file.try_clone()?,stamp:original.stamp.clone(),path:original.path.clone(),mode:original.mode});
        s.uptime=Some(host.uptime.try_clone()?);s.boot=host.boot.clone();
        let (native,child)=UnixStream::pair()?;native.set_nonblocking(true)?;child.set_nonblocking(true)?;
        s.native_stream=Some(native.into());s.child_endpoint=Some(child);s.budget.remaining()?;s.spawn_retaining()?;Ok(())
    }
    fn immutable_current(&self)->Result<()> {
        ensure!(self.boot==storage::proc_bytes("/proc/sys/kernel/random/boot_id",64)?,"upload boot changed");
        check_original_config(self.config.as_ref().context("upload original config absent")?,&self.config_sha)?;
        self.token_stamp.as_ref().context("upload original token absent")?.check(self.token.as_ref().context("upload original token absent")?,0o600)?;
        self.script.as_ref().context("upload original script absent")?.check()?;self.node.as_ref().context("upload original Node absent")?.check()?;Ok(())
    }
    fn spawn_retaining(&mut self)->Result<()> {
        self.budget.remaining()?;self.immutable_current()?;ensure!(!self.attempted,"upload spawn cannot replay");
        let scope=super::super::sealed_data(&self.scope)?;
        let stream:OwnedFd=self.child_endpoint.as_ref().unwrap().try_clone()?.into();let token:OwnedFd=self.token.as_ref().unwrap().try_clone()?.into();
        let uptime:OwnedFd=self.uptime.as_ref().unwrap().try_clone()?.into();
        let config=duplicate_original_config(self.config.as_ref().context("upload original config absent")?,&self.config_sha)?;
        let (mut command,held)=super::super::mapped_command(NODE,&["--no-addons","--disable-proto=throw",UPLOADER],false,&[(&stream,3),(&scope,4),(&token,5),(&uptime,6),(&config,7)])?;
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        unsafe {command.pre_exec(|| {
            if libc::setgroups(0,std::ptr::null())!=0 {return Err(std::io::Error::last_os_error());}
            for cap in 0..=63 {if libc::prctl(libc::PR_CAPBSET_READ,cap,0,0,0)==1&&libc::prctl(libc::PR_CAPBSET_DROP,cap,0,0,0)!=0{return Err(std::io::Error::last_os_error());}}
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS,1,0,0,0)!=0||libc::prctl(libc::PR_SET_KEEPCAPS,0,0,0,0)!=0
                ||libc::prctl(libc::PR_CAP_AMBIENT,libc::PR_CAP_AMBIENT_CLEAR_ALL,0,0,0)!=0
                ||libc::setresgid(1500,1500,1500)!=0||libc::setresuid(1500,1500,1500)!=0{return Err(std::io::Error::last_os_error());}
            super::super::clear_capabilities()?;Ok(())
        });}
        self.attempted=true;
        ensure!(matches!(super::super::spawn_retaining(&mut self.child,&mut command)?,super::super::SpawnEffect::ChildStored),"upload child definite spawn failure");
        drop(held);drop(stream);self.child_endpoint.take();
        let child=self.child.as_mut().unwrap();self.stdout=child.child.stdout.take();self.stderr=child.child.stderr.take();
        for fd in [self.stdout.as_ref().map(AsRawFd::as_raw_fd),self.stderr.as_ref().map(AsRawFd::as_raw_fd)] {
            let fd=fd.context("upload stdio capture absent")?;let flags=unsafe {libc::fcntl(fd,libc::F_GETFL)};
            ensure!(flags>=0&&unsafe {libc::fcntl(fd,libc::F_SETFL,flags|libc::O_NONBLOCK)}==0,"upload stdio nonblock unavailable");}
        let p=child.original()?;self.exe=Some(File::open(format!("/proc/{}/exe",p.pid))?.into());self.node.as_ref().unwrap().matches_fd(self.exe.as_ref().unwrap())?;
        storage::floor(p.pid,1500)?;
        for (name,kind) in [("mnt",libc::CLONE_NEWNS),("user",libc::CLONE_NEWUSER),("net",libc::CLONE_NEWNET),("pid",libc::CLONE_NEWPID)] {
            self.namespaces.push((name.into(),storage::namespace(Some(p.pid),name,kind)?,kind));}
        self.budget.remaining()?;self.current()?;Ok(())
    }
    pub(crate) fn native_fd(&self)->Result<&OwnedFd> {ensure!(!self.delivered,"upload stream already delivered");self.native_stream.as_ref().context("upload original stream absent")}
    pub(crate) fn delivered(&mut self) {self.delivered=true;self.native_stream.take();}
    pub(crate) fn scope(&self)->&UploadScope {&self.scope}
    pub(crate) fn current(&self)->Result<()> {
        self.budget.remaining()?;self.immutable_current()?;let p=self.child.as_ref().context("upload child absent")?.original()?;p.live()?;
        self.node.as_ref().unwrap().matches_process(p)?;storage::floor(p.pid,1500)?;
        for (name,held,kind) in &self.namespaces {ensure!(storage::id(held)?==storage::id(&storage::namespace(Some(p.pid),name,*kind)?)?
            &&storage::id(held)?==storage::id(&storage::namespace(None,name,*kind)?)?,"upload original namespace changed");}Ok(())
    }
    /// Always called even after transport deadline/fence. Cancellation is one
    /// pidfd signal attempt; only actual Child::try_wait and pipe EOF settle.
    pub(crate) fn settle_retaining(&mut self)->Result<bool> {
        let expired=self.budget.remaining().is_err();
        if expired||self.child.is_some()&&self.child.as_ref().unwrap().status.is_none()&&self.current().is_err() {self.failed=true;}
        if let Some(child)=self.child.as_mut() {
            if child.status.is_none(){child.status=child.child.try_wait()?;}
            if child.status.is_none()&&(expired||self.failed)&&!self.kill_attempted {self.kill_attempted=true;
                let p=child.original()?;ensure!(unsafe {libc::syscall(libc::SYS_pidfd_send_signal,p.fd().as_raw_fd(),libc::SIGKILL,std::ptr::null::<libc::siginfo_t>(),0)}==0,"upload original cancellation uncertain");}
            drain(&mut self.stdout,&mut self.out,&mut self.out_eof)?;drain(&mut self.stderr,&mut self.err,&mut self.err_eof)?;
            Ok(child.status.is_some()&&self.out_eof&&self.err_eof)
        }else{Ok(false)}
    }
    pub(crate) fn successful(&mut self)->Result<bool> {
        if !self.settle_retaining()? {return Ok(false);}ensure!(!self.failed&&self.child.as_ref().unwrap().status.as_ref().unwrap().success()&&self.err.is_empty(),"upload child failed; original retained");
        let value:serde_json::Value=serde_json::from_slice(&self.out).map_err(|_|anyhow::anyhow!("upload child receipt invalid"))?;
        ensure!(value==serde_json::json!({"format_version":1,"transfer_id":self.scope.transfer_id,"request_nonce":self.scope.request_nonce,"http_settled":true,"source_eof":true}),"upload child receipt differs");Ok(true)
    }
}
fn drain<T:Read>(pipe:&mut Option<T>,bytes:&mut Vec<u8>,eof:&mut bool)->Result<()> {
    if *eof{return Ok(());}let p=pipe.as_mut().context("upload stdio owner absent")?;let mut chunk=[0;1024];
    for _ in 0..4 {match p.read(&mut chunk){Ok(0)=>{*eof=true;return Ok(());},Ok(n)=>{ensure!(bytes.len()+n<=4096,"upload stdio exceeds bound");bytes.extend_from_slice(&chunk[..n]);},
        Err(e)if e.kind()==std::io::ErrorKind::WouldBlock=>return Ok(()),Err(e)if e.kind()==std::io::ErrorKind::Interrupted=>continue,Err(_)=>return Err(anyhow::anyhow!("upload stdio settlement unavailable"))}}
    Ok(())
}

#[cfg(test)]
#[path="qualified_control_origin_tests.rs"]
mod qualified_control_origin_tests;
