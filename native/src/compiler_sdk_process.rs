//! Actual Root-owned UID1500 SDK process. The same private producer serves
//! the startup fixed qualifier and exact ordinary selected-source invocations.
use super::*;
use std::io::Read;
pub(super) struct SdkProcess {proof:Arc<RuntimeBootstrapProof>,admission:Arc<RuntimeReadAdmission>,original:FinalizationBudget,
    root:Option<OwnedFd>,data:Option<CompilerSdkMountData>,helper:Helper,job:Option<serde_json::Value>,job_fds:Option<(OwnedFd,OwnedFd)>,
    job_child_disposal:Option<child_settlement::ChildSettlement>,terminal_failure:Option<terminal::OriginalHelperTerminalFailure>,pipe_error:bool,stdout:Option<std::process::ChildStdout>,stderr:Option<std::process::ChildStderr>,stdout_raw:Vec<u8>,stderr_raw:Vec<u8>,stdout_eof:bool,stderr_eof:bool,cancel:Option<OwnedFd>}
impl SdkProcess {
    pub(super) fn capture_retaining(out:&mut Option<Self>,proof:Arc<RuntimeBootstrapProof>,admission:Arc<RuntimeReadAdmission>,original:FinalizationBudget,
        root:OwnedFd,data:CompilerSdkMountData,job:Option<serde_json::Value>,job_fds:Option<(OwnedFd,OwnedFd)>,cancel:Option<OwnedFd>)->Result<()> {
        ensure!(out.is_none(),"original SDK process carrier occupied");*out=Some(Self{proof,admission,original,root:Some(root),data:Some(data),helper:Helper::staged(),job,job_fds,
            job_child_disposal:None,terminal_failure:None,pipe_error:false,stdout:None,stderr:None,stdout_raw:Vec::new(),stderr_raw:Vec::new(),stdout_eof:false,stderr_eof:false,cancel});Ok(())
    }
    fn current_build(&self)->Result<()>{self.original.remaining()?;if let Some(fd)=&self.cancel{check_cancel(fd)?;}self.admission.current()?;self.proof.current()?;
        ensure!(wire::fd_data(self.root.as_ref().unwrap())?==self.data.as_ref().unwrap().floor,"SDK original execution floor changed");Ok(())}
    pub(super) fn result_data(&self)->Result<&CompilerSdkProbeData>{ensure!(self.helper.exit.as_ref().is_some_and(ExitStatus::success)&&self.stdout_eof&&self.stderr_eof,"SDK actual exit/full pipe EOF absent");self.helper.result.as_ref().context("SDK actual original result absent")}
    fn drain_retaining(&mut self)->Result<()> {
        fn read(fd:i32,raw:&mut Vec<u8>,eof:&mut bool)->Result<()>{if *eof{return Ok(())}let mut bytes=[0u8;4096];for _ in 0..16{let n=unsafe{libc::read(fd,bytes.as_mut_ptr().cast(),bytes.len())};
            if n==0{*eof=true;break}if n<0{let e=io::Error::last_os_error();if e.kind()==io::ErrorKind::WouldBlock{break}if e.kind()==io::ErrorKind::Interrupted{continue}return Err(e.into())}ensure!(raw.len()+n as usize<=65536,"SDK actual pipe bound exceeded");raw.extend_from_slice(&bytes[..n as usize]);}Ok(())}
        let result=(||->Result<()>{if let Some(fd)=&self.stdout{read(fd.as_raw_fd(),&mut self.stdout_raw,&mut self.stdout_eof)?;}if let Some(fd)=&self.stderr{read(fd.as_raw_fd(),&mut self.stderr_raw,&mut self.stderr_eof)?;}Ok(())})();
        if result.is_err(){self.pipe_error=true;}result
    }
    pub(super) fn run_retaining(&mut self)->Result<()> {self.start_retaining()?;loop{if self.poll_retaining()?{return Ok(())}std::thread::sleep(self.original.remaining()?.min(Duration::from_millis(1)));}}
    pub(super) fn poll_retaining(&mut self)->Result<bool>{self.current_build()?;self.poll_helper()}
    pub(super) fn start_retaining(&mut self)->Result<()> {
        ensure!(!self.helper.attempted,"SDK qualification helper cannot retry");self.helper.attempted=true;self.current_build()?;
        let listener=unsafe{libc::socket(libc::AF_UNIX,libc::SOCK_SEQPACKET|libc::SOCK_NONBLOCK|libc::SOCK_CLOEXEC,0)};
        ensure!(listener>=0,"SDK original listener unavailable");self.helper.listener=Some(unsafe{OwnedFd::from_raw_fd(listener)});
        let(address,len)=abstract_address(&self.helper.name)?;ensure!(unsafe{libc::bind(listener,(&address as *const libc::sockaddr_un).cast(),len)}==0&&unsafe{libc::listen(listener,4)}==0,"SDK private listener unavailable");
        let log=OpenOptions::new().write(true).create_new(true).mode(0o600).open(Path::new(SDK_ROOT).join(format!("{}.stderr",self.helper.name)))?;self.helper.stderr=Some(log);
        let work=PathBuf::from(format!("/workspace/rudder-compiler-sdk-{}",uuid::Uuid::new_v4()));
        self.helper.work_path=Some(work.clone());fs::DirBuilder::new().mode(0o700).create(&work)?;
        self.helper.work=Some(OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY|libc::O_NOFOLLOW|libc::O_CLOEXEC).open(&work)?);
        ensure!(unsafe{libc::fchown(self.helper.work.as_ref().unwrap().as_raw_fd(),1500,1500)}==0,"SDK actual private helper directory owner unavailable");
        let mut command=Command::new(ENGINE_PATH);command.args([HELPER_ARG,&self.helper.name]).env_clear().current_dir(&work).stdin(Stdio::null()).stdout(Stdio::piped())
            .stderr(Stdio::piped()).uid(1500).gid(1500);
        unsafe{command.pre_exec(||{if libc::setgroups(0,std::ptr::null())!=0{return Err(io::Error::last_os_error())}Ok(())});}
        self.helper.child=Some(command.spawn()?);
        self.stdout=self.helper.child.as_mut().unwrap().stdout.take();self.stderr=self.helper.child.as_mut().unwrap().stderr.take();
        for fd in [self.stdout.as_ref().unwrap().as_raw_fd(),self.stderr.as_ref().unwrap().as_raw_fd()]{let f=unsafe{libc::fcntl(fd,libc::F_GETFL)};ensure!(f>=0&&unsafe{libc::fcntl(fd,libc::F_SETFL,f|libc::O_NONBLOCK)}==0,"SDK actual pipe nonblocking unavailable");}
        let pid=self.helper.child.as_ref().unwrap().id();
        let raw=unsafe{libc::syscall(libc::SYS_pidfd_open,pid,0)}as i32;ensure!(raw>=0,"SDK original helper pidfd outcome unknown");
        self.helper.pidfd=Some(unsafe{OwnedFd::from_raw_fd(raw)});self.helper.process=Some(Process::capture(self.helper.pidfd.as_ref().unwrap().try_clone()?)?);self.helper.start=Some(original_stat(pid)?.1);
        self.current_build()?;Ok(())
    }
    fn poll_helper(&mut self)->Result<bool>{
        self.drain_retaining()?;
        if self.helper.exit.is_none(){self.helper.exit=self.helper.child.as_mut().context("actual SDK Child absent")?.try_wait()?;}
        if let Some(exit)=&self.helper.exit{ensure!(exit.success(),"SDK actual original helper failed: {exit}");}
        let helper=self.helper.process.as_ref().context("actual SDK helper pidfd absent")?;
        if self.helper.endpoint.is_none(){let fd=unsafe{libc::accept4(self.helper.listener.as_ref().unwrap().as_raw_fd(),std::ptr::null_mut(),std::ptr::null_mut(),libc::SOCK_CLOEXEC|libc::SOCK_NONBLOCK)};
            if fd<0{ensure!(io::Error::last_os_error().kind()==io::ErrorKind::WouldBlock,"SDK original helper accept failed");helper.live()?;return Ok(false)}
            self.helper.accepted.push(unsafe{OwnedFd::from_raw_fd(fd)});let fd=self.helper.accepted.last().unwrap();self.helper.peer=Some(peer_pidfd(fd.as_raw_fd())?);self.helper.endpoint=Some(RuntimeEndpoint::capture(fd.try_clone()?)?);
            let peer=Process::capture(self.helper.peer.as_ref().unwrap().try_clone()?)?;ensure!(peer.pid==helper.pid,"SDK foreign helper connection retained/refused");
        }
        if !self.helper.sent{
            helper.live()?;storage::floor(helper.pid,1500)?;self.proof.engine_image.matches_process(helper)?;wire::require_initial_id_maps(helper.pid)?;
            ensure!(original_stat(helper.pid)?.0==self.proof.own.pid&&fs::metadata(format!("/proc/{}",helper.pid))?.uid()==0,"SDK original helper parent/ND0 differs");
            let data=serde_json::json!({"version":1,"kind":"root-compiler-sdk-fixed-helper-v1","service_pid":self.proof.own.pid,"launcher_pid":self.proof.root.pid,
                "helper_pid":helper.pid,"helper_start_ticks":self.helper.start,"constraint":self.original.constraint(),"mount":self.data,"job":self.job,"service_sha256":self.proof.service_image.sha256,"launcher_sha256":self.proof.root_image.sha256});
            if self.helper.bytes.is_none(){self.helper.bytes=Some(canonical_json(&data)?);ensure!(self.helper.bytes.as_ref().unwrap().len()<=16*1024,"SDK original helper capsule exceeds fixed bound");self.helper.certificate=Some(super::super::sealed_bytes(self.helper.bytes.as_ref().unwrap())?);
                self.helper.fds.push(self.helper.certificate.as_ref().unwrap().try_clone()?);self.helper.fds.push(self.proof.own.fd().try_clone()?);
                self.helper.fds.push(File::open(format!("/proc/{}/exe",self.proof.own.pid))?.into());
                for fd in [self.proof.root.fd(),&self.proof.raw[3],self.root.as_ref().unwrap()]{self.helper.fds.push(fd.try_clone()?);}
                self.helper.fds.push(self.helper.work.as_ref().unwrap().try_clone()?.into());
                if let Some((source,output))=&self.job_fds{self.helper.fds.push(source.try_clone()?);self.helper.fds.push(output.try_clone()?);if let Some(fd)=&self.cancel{self.helper.fds.push(fd.try_clone()?);}}}
            if !self.helper.endpoint.as_ref().unwrap().send_data(self.helper.bytes.as_ref().unwrap(),&self.helper.fds.iter().collect::<Vec<_>>())?{return Ok(false)}self.helper.sent=true;
        }
        if self.helper.pending.is_none(){match self.helper.endpoint.as_ref().unwrap().receive_disposal_retaining(&mut self.helper.pending)?{
            wire::RuntimeDisposalRead::Pending=>{},wire::RuntimeDisposalRead::Packet=>{},wire::RuntimeDisposalRead::ReadClosed{..}=>anyhow::bail!("SDK original helper endpoint closed without result; owners retained")}}
        if self.helper.result.is_none(){let Some(packet)=self.helper.pending.as_ref()else{return Ok(false)};
            ensure!(packet.credentials.pid==helper.pid as i32&&packet.credentials.uid==1500&&packet.credentials.gid==1500&&packet.fds.is_empty(),"SDK fixed helper result origin differs");
            let result:CompilerSdkProbeData=serde_json::from_slice(&packet.bytes)?;ensure!(result.format_version==1&&result.kind=="original-sdk-fixed-helper-complete-v1"&&result.helper_pid==helper.pid&&result.helper_start_ticks==*self.helper.start.as_ref().unwrap()&&result.original_constraint==self.original.constraint(),"SDK fixed helper result binding differs");
            if self.job.is_none(){validate_workload(&result.workload,&self.original.constraint())?;}else{ensure!(result.workload["kind"]=="actual-selected-sdk-build-v1"&&result.workload["action_key"]==self.job.as_ref().unwrap()["action_key"],"SDK selected result differs");validate_selected_rules_workload(&result.workload,self.job.as_ref().unwrap())?;}
            let data=self.data.as_ref().unwrap();let actual=&result.workload["sdk_python"];
            ensure!(actual["sha256"]==data.toolchains["python"].configuration["executable_raw_sha256"]&&actual["device"]==data.floor.device.to_string()&&result.workload["sdk_root_device"]==data.floor.device.to_string()&&result.workload["sdk_root_readonly"]==true,"SDK actual interpreter is not the qualified SDK floor");
            for key in ["user_namespace","mount_namespace","network_namespace","pid_namespace"]{let root=match key{"user_namespace"=>"user","mount_namespace"=>"mnt","network_namespace"=>"net",_=>"pid"};ensure!(result.workload[key]!=fs::read_link(format!("/proc/self/ns/{root}"))?.display().to_string(),"SDK compiler did not enter its own actual namespace");}
            for key in ["CapEff","CapPrm","CapInh","CapAmb","CapBnd"]{ensure!(result.workload["compiler_capabilities"][key]=="0000000000000000","SDK actual compiler capabilities differ");}
            let start:u64=result.workload["started_boottime_nanoseconds"].as_str().context("SDK actual compiler start absent")?.parse()?;
            let end:u64=result.workload["completed_boottime_nanoseconds"].as_str().context("SDK actual compiler end absent")?.parse()?;
            let(cutoff,_)=self.original.observed_clock_bounds()?;ensure!(start>0&&start<=end&&end<cutoff,"SDK actual compiler left original clock");
            self.helper.result=Some(result);
        }
        if self.helper.exit.is_none(){self.helper.exit=self.helper.child.as_mut().unwrap().try_wait()?;}let Some(exit)=self.helper.exit.as_ref()else{return Ok(false)};
        ensure!(exit.success(),"SDK original helper failed after retained result");self.drain_retaining()?;if !self.stdout_eof||!self.stderr_eof{return Ok(false)}self.current_build()?;Ok(true)
    }
}

#[path="compiler_sdk_process_disposal.rs"]
pub(super) mod disposal;

#[path="compiler_sdk_child_settlement.rs"]
mod child_settlement;

#[path="compiler_sdk_terminal_process.rs"]pub(super) mod terminal;
