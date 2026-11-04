//! The fixed startup helper has no per-job Native disposal lane. Its original
//! authenticated result plus actual Child wait is required before closing its
//! aliases. No failure, EOF, PID or cancellation substitutes for that result.
use super::*;
pub(super) struct StartupRetirement {original:FinalizationBudget,ready:bool}
impl SdkProcess {
    pub(in crate::root_storage_bootstrap::runtime_image::compiler_sdk) fn retire_startup_retaining(&mut self,original:&FinalizationBudget,cancel:&AtomicBool)->Result<()> {
        original.remaining()?;ensure!(!cancel.load(Ordering::Acquire),"SDK startup retirement cancelled");
        ensure!(self.job.is_none(),"ordinary SDK job requires its original Native disposal owner");
        if let Some(retained)=&self.startup_retirement {
            ensure!(retained.original.constraint()==original.constraint(),"SDK startup retirement clock replaced");
            if retained.ready{return Ok(())}
        }else{self.startup_retirement=Some(StartupRetirement{original:original.clone(),ready:false});}
        self.proof.current()?;self.admission.current()?;self.result_data()?;
        ensure!(self.helper.sent&&!self.helper.capsule_failed&&!self.pipe_error,"SDK startup original authenticated result unavailable");
        if self.job_child_disposal.is_none(){
            let child=self.helper.child.as_mut().context("SDK original startup Child absent")?;
            let exit=child.try_wait()?.context("SDK startup Child remains live")?;
            let process=self.helper.process.as_ref().context("SDK original startup pidfd absent")?;
            ensure!(exit.success()&&self.helper.exit==Some(exit)&&child.id()==process.pid&&process.exited()?,"SDK startup original wait identity differs");
            child_settlement::ChildSettlement::stage_retaining(&mut self.job_child_disposal,&mut self.helper.child,&mut self.stdout,&mut self.stderr)?;
            self.job_child_disposal.as_mut().unwrap().run_retaining(original)?;
        }
        self.job_child_disposal.as_ref().unwrap().original_pid_data()?;
        original.remaining()?;ensure!(!cancel.load(Ordering::Acquire),"SDK startup retirement cancelled; originals retained");
        self.root.take();self.cancel.take();self.helper.process.take();self.helper.pidfd.take();self.helper.peer.take();self.helper.endpoint.take();self.helper.listener.take();
        self.helper.accepted.clear();self.helper.fds.clear();self.helper.certificate.take();self.helper.stderr.take();self.helper.work.take();
        self.startup_retirement.as_mut().unwrap().ready=true;Ok(())
    }
}
