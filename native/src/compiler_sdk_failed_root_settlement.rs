//! Authenticated closure of only an actually observed failed HELPER job.
//! Distinct from artifact publication/success, with the SAME original private
//! channel, Native thread/TLS issuer, and Root actual Child/EOF observations.
use super::*;
use super::super::super::sdk_job_protocol::{FailureCloseRequest,FailureCloseAck};
use rudder_engine::execution::runtime_image::RuntimeDisposalRead;
pub(super) struct FailedRootJobDisposal {packet:Option<RuntimePacket>,request:Option<FailureCloseRequest>,original:Option<FinalizationBudget>,
    extra:Option<RuntimePacket>,native_write_eof:bool,process:Option<sdk_process::terminal::FailedSdkProcessDisposed>,ack:Option<Vec<u8>>,sent:bool,ready:bool,error:Option<anyhow::Error>}
impl FailedRootJobDisposal {fn staged()->Self{Self{packet:None,request:None,original:None,extra:None,native_write_eof:false,process:None,ack:None,sent:false,ready:false,error:None}}}
impl RootBuildTransfer {
    pub(super) fn failed_resources_disposed(&self)->bool{self.failed_disposal.as_ref().is_some_and(|d|d.ready)}
    pub(super) fn poll_failed_job_disposal_retaining(&mut self,owner:&RuntimeImageMountOwner)->Result<bool>{
        // A queued/late/unknown failure packet never becomes an original ACK.
        if !self.terminal_ready||self.terminal_error.is_some(){return Ok(false)}
        if self.failed_disposal.is_none(){self.failed_disposal=Some(FailedRootJobDisposal::staged())}
        let binding=self.original_binding()?;let observed=self.terminal_ack.as_ref().context("original failed helper ACK absent")?.helper.clone();
        let disposal=self.failed_disposal.as_mut().unwrap();if disposal.ready||disposal.error.is_some(){return Ok(false)}
        let result=(||->Result<bool>{owner.current()?;let sdk=owner.compiler_sdk.as_ref().context("original SDK floor absent")?;sdk.current()?;
            if disposal.packet.is_none(){match self.reply.as_ref().context("original failed SDK reply absent")?.receive_disposal_retaining(&mut disposal.packet)?{
                RuntimeDisposalRead::Pending=>return Ok(false),RuntimeDisposalRead::Packet=>{},RuntimeDisposalRead::ReadClosed{..}=>anyhow::bail!("SDK failed caller closed without original disposal request")}}
            if disposal.request.is_none(){let packet=disposal.packet.as_ref().unwrap();ensure!(packet.credentials.pid==owner.proof.engine.pid as i32&&packet.credentials.uid==1500&&packet.credentials.gid==1500&&packet.fds.len()==1,"SDK failure disposal is not original Native issuer");
                disposal.request=Some(serde_json::from_slice(&packet.bytes)?);let request=disposal.request.as_ref().unwrap();request.validate()?;
                ensure!(request.binding==binding&&request.helper==observed,"SDK failed disposal belongs to another actual job/observation");
                disposal.original=Some(FinalizationBudget::from_constraint(&request.cleanup_constraint)?);
            }
            let original=disposal.original.as_ref().unwrap();let cancel=&disposal.packet.as_ref().unwrap().fds[0];original.remaining()?;check_cancel(cancel)?;
            if !disposal.native_write_eof{match self.reply.as_ref().unwrap().receive_disposal_retaining(&mut disposal.extra)?{
                RuntimeDisposalRead::Pending=>return Ok(false),RuntimeDisposalRead::Packet=>anyhow::bail!("SDK failed disposal caller sent extra packet/descriptors"),RuntimeDisposalRead::ReadClosed{..}=>disposal.native_write_eof=true}}
            if disposal.process.is_none(){self.process.as_mut().context("actual original failed SDK process absent")?.dispose_failed_job_resources_retaining(&mut disposal.process,&observed,original,cancel)?;}
            // Close every original transferred source/out/cancel/reply alias,
            // in addition to those held by the actual process helper.
            self.packet.fds.clear();original.remaining()?;check_cancel(cancel)?;sdk.current()?;
            if disposal.ack.is_none(){let(pid,start)=disposal.process.as_ref().unwrap().original_helper_data()?;
                disposal.ack=Some(canonical_json(&FailureCloseAck{version:1,kind:"compiler_sdk_failed_job_resources_closed_v1".into(),request:disposal.request.as_ref().unwrap().clone(),original_helper_pid:pid,original_helper_start:start.into()})?);}
            if !disposal.sent{if !self.reply.as_ref().unwrap().send_data(disposal.ack.as_ref().unwrap(),&[])?{return Ok(false)}disposal.sent=true;}
            original.remaining()?;check_cancel(cancel)?;sdk.current()?;owner.proof.current()?;
            self.reply.take();original.remaining()?;check_cancel(cancel)?;
            disposal.packet.as_mut().unwrap().fds.clear();disposal.ready=true;Ok(true)
        })();if let Err(error)=result{disposal.error=Some(error);return Ok(false)}result
    }
}
