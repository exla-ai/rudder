//! Actual Root-owned UID1500 SDK process. The same private producer serves
//! the startup fixed qualifier and exact ordinary selected-source invocations.
use super::*;
use std::io::Read;
// Command::uid/gid run before pre_exec. Keep the forked child Root until
// CAP_SETPCAP has removed every bounding bit, then irreversibly drop all IDs.
// This runs only in Command's child: syscalls and errno conversion, no locks,
// allocation, environment, filesystem access, new budget or retry.
unsafe fn sdk_helper_exec_floor()->io::Result<()> {
    if libc::syscall(libc::SYS_setgroups,0usize,std::ptr::null::<libc::gid_t>())!=0{return Err(io::Error::last_os_error())}
    crate::root_storage_bootstrap::restrict_bounding(0)?;
    if libc::prctl(libc::PR_SET_KEEPCAPS,0,0,0,0)!=0
        ||libc::prctl(libc::PR_CAP_AMBIENT,libc::PR_CAP_AMBIENT_CLEAR_ALL,0,0,0)!=0
        ||libc::prctl(libc::PR_SET_NO_NEW_PRIVS,1,0,0,0)!=0
        ||libc::syscall(libc::SYS_setresgid,1500u32,1500u32,1500u32)!=0||libc::syscall(libc::SYS_setresuid,1500u32,1500u32,1500u32)!=0 {
        return Err(io::Error::last_os_error())
    }
    crate::root_storage_bootstrap::clear_capabilities()?;Ok(())
}
// Diagnostic observation only. A closed startup receive direction cannot
// acquire a result, reopen a handshake, settle a child or create a new clock.
#[derive(Default)]
struct StartupResultReadClosure {observed:bool}
impl StartupResultReadClosure {
    fn observe(&mut self,startup:bool)->Result<()> {
        ensure!(startup,"SDK original helper endpoint closed without result; owners retained");
        self.observed=true;Ok(())
    }
    fn poll_original(&self,exit:Option<&ExitStatus>,stdout_eof:bool,stderr_eof:bool)->Result<bool> {
        ensure!(self.observed,"SDK startup read closure was not observed");
        // Nonzero exits must go through the existing SAME Child/pidfd/wait/
        // EOF validation above this call before any fixed stderr projection.
        ensure!(exit.is_none_or(ExitStatus::success)||!stdout_eof||!stderr_eof,"SDK failed original helper identity checks remain required");
        if exit.is_some()&&stdout_eof&&stderr_eof{return Err(anyhow::Error::new(BootstrapCheck::SdkHelperExitUnknown))}
        Ok(false)
    }
}
pub(super) struct SdkProcess {proof:Arc<RuntimeBootstrapProof>,admission:Arc<RuntimeReadAdmission>,original:FinalizationBudget,
    diagnostic_check:BootstrapCheck,startup_read_closure:StartupResultReadClosure,root:Option<OwnedFd>,data:Option<CompilerSdkMountData>,helper:Helper,job:Option<serde_json::Value>,job_fds:Option<(OwnedFd,OwnedFd)>,
    job_child_disposal:Option<child_settlement::ChildSettlement>,terminal_failure:Option<terminal::OriginalHelperTerminalFailure>,pipe_error:bool,stdout:Option<std::process::ChildStdout>,stderr:Option<std::process::ChildStderr>,stdout_raw:Vec<u8>,stderr_raw:Vec<u8>,stdout_eof:bool,stderr_eof:bool,cancel:Option<OwnedFd>}
impl SdkProcess {
    pub(super) fn capture_retaining(out:&mut Option<Self>,proof:Arc<RuntimeBootstrapProof>,admission:Arc<RuntimeReadAdmission>,original:FinalizationBudget,
        root:OwnedFd,data:CompilerSdkMountData,job:Option<serde_json::Value>,job_fds:Option<(OwnedFd,OwnedFd)>,cancel:Option<OwnedFd>)->Result<()> {
        ensure!(out.is_none(),"original SDK process carrier occupied");*out=Some(Self{proof,admission,original,diagnostic_check:BootstrapCheck::SdkProcessOriginal,startup_read_closure:StartupResultReadClosure::default(),root:Some(root),data:Some(data),helper:Helper::staged(),job,job_fds,
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
    pub(super) fn run_retaining(&mut self)->Result<()> {self.start_retaining()?;loop{if self.poll_retaining()?{return Ok(())}std::thread::sleep(self.original.remaining().map_err(|error|bootstrap_failure_context(error,BootstrapCheck::SdkProcessOriginal))?.min(Duration::from_millis(1)));}}
    pub(super) fn poll_retaining(&mut self)->Result<bool>{
        self.diagnostic_check=BootstrapCheck::SdkProcessOriginal;
        let result=(||{self.current_build()?;self.poll_helper()})();
        result.map_err(|error|bootstrap_failure_context(error,self.diagnostic_check))
    }
    pub(super) fn start_retaining(&mut self)->Result<()> {
        self.diagnostic_check=BootstrapCheck::SdkProcessSpawn;
        self.start_inner().map_err(|error|bootstrap_failure_context(error,self.diagnostic_check))
    }
    fn start_inner(&mut self)->Result<()> {
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
            .stderr(Stdio::piped());
        unsafe{command.pre_exec(||sdk_helper_exec_floor());}
        self.helper.child=Some(command.spawn()?);
        self.diagnostic_check=BootstrapCheck::SdkProcessCapture;
        self.stdout=self.helper.child.as_mut().unwrap().stdout.take();self.stderr=self.helper.child.as_mut().unwrap().stderr.take();
        for fd in [self.stdout.as_ref().unwrap().as_raw_fd(),self.stderr.as_ref().unwrap().as_raw_fd()]{let f=unsafe{libc::fcntl(fd,libc::F_GETFL)};ensure!(f>=0&&unsafe{libc::fcntl(fd,libc::F_SETFL,f|libc::O_NONBLOCK)}==0,"SDK actual pipe nonblocking unavailable");}
        let pid=self.helper.child.as_ref().unwrap().id();
        let raw=unsafe{libc::syscall(libc::SYS_pidfd_open,pid,0)}as i32;ensure!(raw>=0,"SDK original helper pidfd outcome unknown");
        self.helper.pidfd=Some(unsafe{OwnedFd::from_raw_fd(raw)});self.helper.process=Some(Process::capture(self.helper.pidfd.as_ref().unwrap().try_clone()?)?);self.helper.start=Some(original_stat(pid)?.1);
        self.current_build()?;Ok(())
    }
    fn poll_helper(&mut self)->Result<bool>{
        ensure!(!self.helper.capsule_failed,"SDK original capsule failure quarantined; owners retained");
        self.diagnostic_check=BootstrapCheck::SdkProcessPipes;
        self.drain_retaining()?;
        self.diagnostic_check=BootstrapCheck::SdkProcessChildExit;
        if self.helper.exit.is_none(){self.helper.exit=self.helper.child.as_mut().context("actual SDK Child absent")?.try_wait()?;}
        if self.helper.exit.as_ref().is_some_and(|exit|!exit.success()){
            // Wait only on these SAME original pipes under current_build's
            // SAME absolute cutoff; no reply/result or settlement is minted.
            if !self.stdout_eof||!self.stderr_eof{return Ok(false)}
            let child=self.helper.child.as_mut().context("original failed SDK Child absent")?;
            let process=self.helper.process.as_ref().context("original failed SDK pidfd absent")?;
            ensure!(child.try_wait()?==self.helper.exit&&child.id()==process.pid&&process.exited()?
                &&self.helper.start.as_ref().is_some_and(|start|!start.is_empty())&&!self.pipe_error,
                "original failed SDK Child identity/wait/EOF differs");
            return Err(handshake_diagnostic::failed_original_stderr(&self.stderr_raw,self.job.is_none()));
        }
        if self.startup_read_closure.observed {
            self.diagnostic_check=BootstrapCheck::SdkProcessResult;
            ensure!(self.job.is_none()&&self.helper.pending.is_none()&&self.helper.result.is_none(),"SDK original startup read closure state differs");
            // No further endpoint read, accept, send or handshake is attempted.
            // current_build and the original pipe/wait observations above are
            // repeated under the SAME absolute cutoff until actual termination.
            return self.startup_read_closure.poll_original(self.helper.exit.as_ref(),self.stdout_eof,self.stderr_eof)
        }
        let helper=self.helper.process.as_ref().context("actual SDK helper pidfd absent")?;
        self.diagnostic_check=BootstrapCheck::SdkProcessPeer;
        if self.helper.endpoint.is_none(){let fd=unsafe{libc::accept4(self.helper.listener.as_ref().unwrap().as_raw_fd(),std::ptr::null_mut(),std::ptr::null_mut(),libc::SOCK_CLOEXEC|libc::SOCK_NONBLOCK)};
            if fd<0{ensure!(io::Error::last_os_error().kind()==io::ErrorKind::WouldBlock,"SDK original helper accept failed");helper.live()?;return Ok(false)}
            self.helper.accepted.push(unsafe{OwnedFd::from_raw_fd(fd)});let fd=self.helper.accepted.last().unwrap();self.helper.peer=Some(peer_pidfd(fd.as_raw_fd())?);self.helper.endpoint=Some(RuntimeEndpoint::capture(fd.try_clone()?)?);
            let peer=Process::capture(self.helper.peer.as_ref().unwrap().try_clone()?)?;ensure!(peer.pid==helper.pid,"SDK foreign helper connection retained/refused");
        }
        if !self.helper.sent{
            self.diagnostic_check=BootstrapCheck::SdkProcessHelperFloor;
            helper.live()?;storage::floor(helper.pid,1500)?;
            self.diagnostic_check=BootstrapCheck::SdkProcessHelperImage;
            self.proof.engine_image.matches_process(helper)?;
            self.diagnostic_check=BootstrapCheck::SdkProcessHelperParent;
            wire::require_initial_id_maps(helper.pid)?;
            ensure!(original_stat(helper.pid)?.0==self.proof.own.pid,"SDK original helper parent differs");
            require_protected_descriptor_directory(helper.pid)?;
            self.diagnostic_check=BootstrapCheck::SdkProcessCapsule;
            if self.helper.bytes.is_none(){
                // Retain issuer/proof and each actual executable before any
                // fallible packet construction. Partial construction refuses.
                self.helper.capsule_failed=true;
                peer_witness::OriginalSdkPeerImages::capture_retaining(&mut self.helper.peer_witness,&self.proof,&self.original)?;
                let witness=self.helper.peer_witness.as_ref().unwrap();
                let data=peer_witness::SdkHelperCapsuleData{version:2,kind:peer_witness::CAPSULE_KIND.into(),service_pid:self.proof.own.pid,launcher_pid:self.proof.root.pid,
                    helper_pid:helper.pid,helper_start_ticks:self.helper.start.as_ref().unwrap().clone(),constraint:self.original.constraint(),mount:self.data.as_ref().unwrap().clone(),
                    job:self.job.clone().unwrap_or(serde_json::Value::Null),service_sha256:self.proof.service_image.sha256.clone(),launcher_sha256:self.proof.root_image.sha256.clone(),peer_image_witness:witness.data()?};
                self.helper.bytes=Some(canonical_json(&data)?);ensure!(self.helper.bytes.as_ref().unwrap().len()<=16*1024,"SDK original helper capsule exceeds fixed bound");
                self.helper.certificate=Some(super::super::sealed_bytes(self.helper.bytes.as_ref().unwrap())?);
                self.helper.fds.push(self.helper.certificate.as_ref().unwrap().try_clone()?);self.helper.fds.push(self.proof.own.fd().try_clone()?);
                self.helper.fds.push(witness.service_fd()?.try_clone()?);self.helper.fds.push(self.proof.root.fd().try_clone()?);self.helper.fds.push(witness.launcher_fd()?.try_clone()?);
                self.helper.fds.push(self.root.as_ref().unwrap().try_clone()?);self.helper.fds.push(self.helper.work.as_ref().unwrap().try_clone()?.into());
                if let Some((source,output))=&self.job_fds{self.helper.fds.push(source.try_clone()?);self.helper.fds.push(output.try_clone()?);if let Some(fd)=&self.cancel{self.helper.fds.push(fd.try_clone()?);}}
                self.helper.capsule_failed=false;
            }
            let witness=self.helper.peer_witness.as_ref().context("SDK original producer witness absent")?;witness.current()?;
            let endpoint=self.helper.endpoint.as_ref().unwrap();let bytes=self.helper.bytes.as_ref().unwrap();let fds=self.helper.fds.iter().collect::<Vec<_>>();
            if !peer_witness::send_once_retaining(&mut self.helper.sent,&mut self.helper.capsule_failed,||endpoint.send_data(bytes,&fds),||witness.current())?{return Ok(false)}
        }
        self.diagnostic_check=BootstrapCheck::SdkProcessResult;
        if self.helper.pending.is_none(){match self.helper.endpoint.as_ref().unwrap().receive_disposal_retaining(&mut self.helper.pending)?{
            wire::RuntimeDisposalRead::Pending=>{},wire::RuntimeDisposalRead::Packet=>{},wire::RuntimeDisposalRead::ReadClosed{..}=>{self.startup_read_closure.observe(self.job.is_none())?;return Ok(false)}}}
        if self.helper.result.is_none(){let Some(packet)=self.helper.pending.as_ref()else{return Ok(false)};
            ensure!(packet.credentials.pid==helper.pid as i32&&packet.credentials.uid==1500&&packet.credentials.gid==1500&&packet.fds.is_empty(),"SDK fixed helper result origin differs");
            let result:CompilerSdkProbeData=serde_json::from_slice(&packet.bytes)?;ensure!(result.format_version==1&&result.kind=="original-sdk-fixed-helper-complete-v1"&&result.helper_pid==helper.pid&&result.helper_start_ticks==*self.helper.start.as_ref().unwrap()&&result.original_constraint==self.original.constraint(),"SDK fixed helper result binding differs");
            self.diagnostic_check=BootstrapCheck::SdkProcessWorkload;
            if self.job.is_none(){validate_workload(&result.workload,&self.original.constraint())?;}else{ensure!(result.workload["kind"]=="actual-selected-sdk-build-v1"&&result.workload["action_key"]==self.job.as_ref().unwrap()["action_key"],"SDK selected result differs");validate_selected_rules_workload(&result.workload,self.job.as_ref().unwrap())?;}
            self.diagnostic_check=BootstrapCheck::SdkProcessInterpreter;
            let data=self.data.as_ref().unwrap();let actual=&result.workload["sdk_python"];
            ensure!(actual["sha256"]==data.toolchains["python"].configuration["executable_raw_sha256"]&&actual["device"]==data.floor.device.to_string()&&result.workload["sdk_root_device"]==data.floor.device.to_string()&&result.workload["sdk_root_readonly"]==true,"SDK actual interpreter is not the qualified SDK floor");
            self.diagnostic_check=BootstrapCheck::SdkProcessNamespaces;
            for key in ["user_namespace","mount_namespace","network_namespace","pid_namespace"]{let root=match key{"user_namespace"=>"user","mount_namespace"=>"mnt","network_namespace"=>"net",_=>"pid"};ensure!(result.workload[key]!=fs::read_link(format!("/proc/self/ns/{root}"))?.display().to_string(),"SDK compiler did not enter its own actual namespace");}
            self.diagnostic_check=BootstrapCheck::SdkProcessCapabilities;
            for key in ["CapEff","CapPrm","CapInh","CapAmb","CapBnd"]{ensure!(result.workload["compiler_capabilities"][key]=="0000000000000000","SDK actual compiler capabilities differ");}
            self.diagnostic_check=BootstrapCheck::SdkProcessClock;
            let start:u64=result.workload["started_boottime_nanoseconds"].as_str().context("SDK actual compiler start absent")?.parse()?;
            let end:u64=result.workload["completed_boottime_nanoseconds"].as_str().context("SDK actual compiler end absent")?.parse()?;
            let(cutoff,_)=self.original.observed_clock_bounds()?;ensure!(start>0&&start<=end&&end<cutoff,"SDK actual compiler left original clock");
            self.helper.result=Some(result);
        }
        self.diagnostic_check=BootstrapCheck::SdkProcessExitEof;
        if self.helper.exit.is_none(){self.helper.exit=self.helper.child.as_mut().unwrap().try_wait()?;}let Some(exit)=self.helper.exit.as_ref()else{return Ok(false)};
        ensure!(exit.success(),"SDK original helper failed after retained result");self.drain_retaining()?;if !self.stdout_eof||!self.stderr_eof{return Ok(false)}self.current_build()?;Ok(true)
    }
}

#[path="compiler_sdk_process_disposal.rs"]
pub(super) mod disposal;

#[path="compiler_sdk_child_settlement.rs"]
mod child_settlement;

#[path="compiler_sdk_terminal_process.rs"]pub(super) mod terminal;

#[cfg(test)]
#[path="compiler_sdk_process_privilege_tests.rs"]
mod privilege_tests;

#[cfg(test)]
#[path="compiler_sdk_handshake_privilege_tests.rs"]
mod handshake_privilege_tests;

#[cfg(test)]
#[path="compiler_sdk_peer_witness_tests.rs"]
mod peer_witness_tests;

#[cfg(test)]
#[path="compiler_sdk_startup_read_closure_tests.rs"]
mod startup_read_closure_tests;
