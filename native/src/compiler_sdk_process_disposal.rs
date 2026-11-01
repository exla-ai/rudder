//! Disposal of ONLY one already-exited ordinary SDK helper and its private
//! descriptor aliases. Shared SDK floor/FUSE/read/TLS owners are untouched.
use super::*;
pub(in crate::root_storage_bootstrap::runtime_image::compiler_sdk) struct SdkProcessJobDisposed {pid:u32,start:String,ready:bool}
impl SdkProcessJobDisposed {
    pub(in crate::root_storage_bootstrap::runtime_image::compiler_sdk) fn original_helper_data(&self)->Result<(u32,&str)>{ensure!(self.ready,"original SDK process descriptor settlement incomplete");Ok((self.pid,&self.start))}
}
impl SdkProcess {
    pub(in crate::root_storage_bootstrap::runtime_image::compiler_sdk) fn dispose_job_resources_retaining(&mut self,out:&mut Option<SdkProcessJobDisposed>,cleanup:&FinalizationBudget,cancel:&OwnedFd)->Result<()> {
        ensure!(out.is_none(),"original SDK process disposal cannot replay");
        *out=Some(SdkProcessJobDisposed{pid:0,start:String::new(),ready:false});cleanup.remaining()?;check_cancel(cancel)?;self.proof.current()?;self.admission.current()?;
        // result_data is an immutable fact from actual original Child wait,
        // authenticated helper result and both pipe EOF. Do NOT call the old
        // build clock/current_build while disposing a completed operation.
        self.result_data()?;ensure!(self.job.is_some()&&self.helper.sent,"ordinary original SDK job absent");
        let child=self.helper.child.as_mut().context("original SDK Child custody absent")?;let actual=child.try_wait()?.context("SDK original child remains live")?;
        ensure!(actual.success()&&self.helper.exit.as_ref()==Some(&actual)&&self.stdout_eof&&self.stderr_eof,"SDK actual original wait/EOF changed");
        let process=self.helper.process.as_ref().context("original SDK pidfd owner absent")?;
        ensure!(process.pid==child.id()&&process.exited()?,"SDK original child/pidfd identity or exit differs");
        let fact=out.as_mut().unwrap();fact.start=self.helper.start.as_ref().context("original SDK process start absent")?.clone();
        child_settlement::ChildSettlement::stage_retaining(&mut self.job_child_disposal,&mut self.helper.child,&mut self.stdout,&mut self.stderr)?;
        self.job_child_disposal.as_mut().unwrap().run_retaining(cleanup)?;fact.pid=self.job_child_disposal.as_ref().unwrap().original_pid_data()?;
        cleanup.remaining()?;check_cancel(cancel)?;
        // No more helper job code can execute: the ACTUAL Child was reaped,
        // its PID-namespace init was waited by the fixed helper, and both of
        // its original pipes reached EOF. There is no per-job Rust/FUSE worker.
        // Paths/diagnostics remain retained; no history/tree/pin is deleted.
        self.stdout.take();self.stderr.take();self.root.take();self.job_fds.take();self.cancel.take();
        self.helper.child.take();self.helper.process.take();self.helper.pidfd.take();self.helper.peer.take();self.helper.endpoint.take();self.helper.listener.take();
        self.helper.accepted.clear();self.helper.fds.clear();self.helper.certificate.take();self.helper.stderr.take();self.helper.work.take();
        cleanup.remaining()?;check_cancel(cancel)?;self.proof.current()?;self.admission.current()?;
        ensure!(self.stdout.is_none()&&self.stderr.is_none()&&self.root.is_none()&&self.job_fds.is_none()&&self.cancel.is_none()
            &&self.helper.child.is_none()&&self.helper.process.is_none()&&self.helper.pidfd.is_none()&&self.helper.peer.is_none()&&self.helper.endpoint.is_none()&&self.helper.listener.is_none()
            &&self.helper.accepted.is_empty()&&self.helper.fds.is_empty()&&self.helper.certificate.is_none()&&self.helper.stderr.is_none()&&self.helper.work.is_none()
            &&self.helper.pending.as_ref().is_none_or(|packet|packet.fds.is_empty()),"SDK job original descriptor alias remains");
        fact.ready=true;Ok(())
    }
}
