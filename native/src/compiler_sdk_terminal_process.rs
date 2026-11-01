//! Passive observation and disposal of the SAME original failed SDK HELPER.
//! A compiler result is never inferred from this outer helper exit. There is
//! no spawn/send retry and no new execution clock in this observer.
use super::*;
use super::super::super::sdk_job_protocol::HelperFailureData;
use std::os::unix::process::ExitStatusExt;
pub(super) struct OriginalHelperTerminalFailure {data:Option<HelperFailureData>,peer_closed:bool,extra:Option<RuntimePacket>,ready:bool}
pub(in crate::root_storage_bootstrap::runtime_image::compiler_sdk) struct FailedSdkProcessDisposed {pid:u32,start:String,ready:bool}
impl FailedSdkProcessDisposed {pub(in crate::root_storage_bootstrap::runtime_image::compiler_sdk) fn original_helper_data(&self)->Result<(u32,&str)>{ensure!(self.ready,"failed SDK process closure incomplete");Ok((self.pid,&self.start))}}
fn diagnostic(raw:&[u8])->String{let text=String::from_utf8_lossy(raw);let mut out=String::new();for ch in text.chars(){if out.len()+ch.len_utf8()>4096{break}out.push(ch);}out}
impl SdkProcess {
    pub(in crate::root_storage_bootstrap::runtime_image::compiler_sdk) fn poll_terminal_failure_retaining(&mut self)->Result<bool>{
        self.proof.current()?;self.admission.current()?;
        // A capture/spawn/handshake failure is NOT an authenticated helper
        // invocation. Its uncertain original remains charged.
        if self.job.is_none()||!self.helper.sent{return Ok(false)}
        ensure!(!self.pipe_error,"SDK original pipe observation failed; retain charge");
        if self.terminal_failure.as_ref().is_some_and(|fact|fact.ready){return Ok(true)}
        if self.terminal_failure.is_none(){self.terminal_failure=Some(OriginalHelperTerminalFailure{data:None,peer_closed:false,extra:None,ready:false});}
        self.drain_retaining()?;
        if self.helper.exit.is_none(){self.helper.exit=self.helper.child.as_mut().context("SDK original failed helper Child absent")?.try_wait()?;}
        let Some(exit)=self.helper.exit else{return Ok(false)};
        // Zero exit without a valid report is still unknown. Malformed result
        // packets/descriptors cannot be repackaged as a terminal failure fact.
        if exit.success(){return Ok(false)}
        ensure!(self.helper.result.is_none()&&self.helper.pending.is_none(),"SDK failed helper has unresolved original reply");
        if !self.stdout_eof||!self.stderr_eof{return Ok(false)}
        let child=self.helper.child.as_mut().context("SDK original failed Child absent")?;
        ensure!(child.try_wait()?==Some(exit),"SDK failed original Child wait differs");
        let process=self.helper.process.as_ref().context("SDK original failed pidfd absent")?;
        ensure!(process.pid==child.id()&&process.exited()?,"SDK original failed helper identity/exit differs");
        let fact=self.terminal_failure.as_mut().unwrap();
        if !fact.peer_closed{match self.helper.endpoint.as_ref().context("SDK actual helper endpoint absent")?.receive_disposal_retaining(&mut fact.extra)?{
            wire::RuntimeDisposalRead::Pending=>return Ok(false),wire::RuntimeDisposalRead::Packet=>anyhow::bail!("SDK failed helper sent unresolved packet/descriptors"),
            wire::RuntimeDisposalRead::ReadClosed{peer_fully_closed:false}=>return Ok(false),wire::RuntimeDisposalRead::ReadClosed{peer_fully_closed:true}=>fact.peer_closed=true}}
        fact.data=Some(HelperFailureData{source_kind:"helper_failure".into(),original_helper_pid:child.id(),original_helper_start:self.helper.start.as_ref().context("original helper start absent")?.clone(),
            exit_code:exit.code(),signal:exit.signal(),stdout_eof:self.stdout_eof,stderr_eof:self.stderr_eof,stdout_bytes:self.stdout_raw.len()as u64,stderr_bytes:self.stderr_raw.len()as u64,
            stdout_sha256:digest(&self.stdout_raw),stderr_sha256:digest(&self.stderr_raw),stderr_diagnostic:diagnostic(&self.stderr_raw)});
        fact.data.as_ref().unwrap().validate()?;self.proof.current()?;self.admission.current()?;
        self.terminal_failure.as_mut().unwrap().ready=true;Ok(true)
    }
    pub(in crate::root_storage_bootstrap::runtime_image::compiler_sdk) fn terminal_reply_current(&self)->Result<()>{self.current_build()}
    pub(in crate::root_storage_bootstrap::runtime_image::compiler_sdk) fn terminal_failure_data(&self)->Result<&HelperFailureData>{let fact=self.terminal_failure.as_ref().context("actual terminal helper observation absent")?;
        ensure!(fact.ready&&fact.peer_closed&&!self.pipe_error,"SDK original failed helper observation incomplete");fact.data.as_ref().context("actual failed helper data absent")}
    pub(in crate::root_storage_bootstrap::runtime_image::compiler_sdk) fn dispose_failed_job_resources_retaining(&mut self,out:&mut Option<FailedSdkProcessDisposed>,expected:&HelperFailureData,cleanup:&FinalizationBudget,cancel:&OwnedFd)->Result<()>{
        ensure!(out.is_none(),"failed SDK process closure cannot replay");*out=Some(FailedSdkProcessDisposed{pid:0,start:String::new(),ready:false});
        cleanup.remaining()?;check_cancel(cancel)?;self.proof.current()?;self.admission.current()?;
        ensure!(self.terminal_failure_data()?==expected&&self.job.is_some()&&self.helper.sent,"SDK failed disposal differs from original observed helper");
        let child=self.helper.child.as_mut().context("actual failed Child custody absent")?;let exit=child.try_wait()?.context("SDK original failed Child live")?;
        ensure!(!exit.success()&&self.helper.exit==Some(exit)&&exit.code()==expected.exit_code&&exit.signal()==expected.signal&&child.id()==expected.original_helper_pid,"SDK failed Child actual wait differs");
        let process=self.helper.process.as_ref().context("original failed pidfd custody absent")?;ensure!(process.pid==child.id()&&process.exited()?,"SDK failed Child/pidfd differs");
        ensure!(self.helper.start.as_deref()==Some(expected.original_helper_start.as_str())&&self.stdout_eof&&self.stderr_eof,"SDK failed original start/pipe EOF differs");
        child_settlement::ChildSettlement::stage_retaining(&mut self.job_child_disposal,&mut self.helper.child,&mut self.stdout,&mut self.stderr)?;
        self.job_child_disposal.as_mut().unwrap().run_terminal_failure_retaining(cleanup)?;
        let fact=out.as_mut().unwrap();fact.pid=self.job_child_disposal.as_ref().unwrap().original_failed_pid_data()?;fact.start=expected.original_helper_start.clone();
        cleanup.remaining()?;check_cancel(cancel)?;
        // Close ONLY per-job originals/aliases after actual nonzero Child wait,
        // both pipe EOF, and actual helper endpoint full closure. No shared
        // floor, FUSE, actor, read worker, pin or filesystem tree is retired.
        self.stdout.take();self.stderr.take();self.root.take();self.job_fds.take();self.cancel.take();
        self.helper.child.take();self.helper.process.take();self.helper.pidfd.take();self.helper.peer.take();self.helper.endpoint.take();self.helper.listener.take();
        self.helper.accepted.clear();self.helper.fds.clear();self.helper.certificate.take();self.helper.stderr.take();self.helper.work.take();
        cleanup.remaining()?;check_cancel(cancel)?;self.proof.current()?;self.admission.current()?;
        ensure!(self.stdout.is_none()&&self.stderr.is_none()&&self.root.is_none()&&self.job_fds.is_none()&&self.cancel.is_none()
            &&self.helper.child.is_none()&&self.helper.process.is_none()&&self.helper.pidfd.is_none()&&self.helper.peer.is_none()&&self.helper.endpoint.is_none()&&self.helper.listener.is_none()
            &&self.helper.accepted.is_empty()&&self.helper.fds.is_empty()&&self.helper.certificate.is_none()&&self.helper.stderr.is_none()&&self.helper.work.is_none(),"SDK failed job descriptor alias remains");
        fact.ready=true;Ok(())
    }
}
#[cfg(test)]mod tests {use super::*;
    #[test]fn actual_held_stderr_preview_is_bounded_utf8_without_claiming_compiler_exit(){let raw=vec![0xff;65536];let text=diagnostic(&raw);assert!(text.len()<=4096);assert!(text.chars().all(|c|c=='\u{fffd}'));}
}
