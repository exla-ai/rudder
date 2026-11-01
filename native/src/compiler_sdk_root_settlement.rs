//! Authenticated close of one actual Root SDK job. A build ACK alone cannot
//! release its capacity; publication and original Native caller are retained
//! by the fixed Native issuer, while Root independently proves process/EOF.
use super::*;
use super::super::super::sdk_job_protocol::{CloseRequest,CloseAck,JobBinding};
use rudder_engine::execution::runtime_image::RuntimeDisposalRead;

pub(super) struct RootJobDisposal {packet:Option<RuntimePacket>,request:Option<CloseRequest>,original:Option<FinalizationBudget>,
    extra:Option<RuntimePacket>,native_write_eof:bool,process:Option<sdk_process::disposal::SdkProcessJobDisposed>,ack:Option<Vec<u8>>,sent:bool,ready:bool,error:Option<anyhow::Error>}
impl RootJobDisposal {fn staged()->Self{Self{packet:None,request:None,original:None,extra:None,native_write_eof:false,process:None,ack:None,sent:false,ready:false,error:None}}}
impl RootBuildTransfer {
    pub(in crate::root_storage_bootstrap) fn resources_disposed(&self)->bool{self.disposal.as_ref().is_some_and(|d|d.ready)||self.failed_resources_disposed()}
    pub(super) fn poll_job_disposal_retaining(&mut self,owner:&RuntimeImageMountOwner)->Result<bool>{
        if self.disposal.is_none(){self.disposal=Some(RootJobDisposal::staged())}
        let binding=self.original_binding()?;
        let disposal=self.disposal.as_mut().unwrap();if disposal.ready||disposal.error.is_some(){return Ok(false)}
        let result=(||->Result<bool>{owner.current()?;let sdk=owner.compiler_sdk.as_ref().context("original SDK floor absent")?;sdk.current()?;
            if disposal.packet.is_none(){match self.reply.as_ref().context("original SDK reply socket absent")?.receive_disposal_retaining(&mut disposal.packet)?{
                RuntimeDisposalRead::Pending=>return Ok(false),RuntimeDisposalRead::Packet=>{},RuntimeDisposalRead::ReadClosed{..}=>anyhow::bail!("SDK private caller closed without original disposal request")}}
            if disposal.request.is_none(){let packet=disposal.packet.as_ref().unwrap();ensure!(packet.credentials.pid==owner.proof.engine.pid as i32&&packet.credentials.uid==1500&&packet.credentials.gid==1500&&packet.fds.len()==1,"SDK disposal is not original authenticated Native issuer");
                disposal.request=Some(serde_json::from_slice(&packet.bytes)?);let request=disposal.request.as_ref().unwrap();request.validate()?;
                ensure!(request.binding==binding,"SDK disposal belongs to another original job");
                disposal.original=Some(FinalizationBudget::from_constraint(&request.cleanup_constraint)?);
            }
            let original=disposal.original.as_ref().unwrap();let cancel=&disposal.packet.as_ref().unwrap().fds[0];original.remaining()?;check_cancel(cancel)?;
            if !disposal.native_write_eof{match self.reply.as_ref().unwrap().receive_disposal_retaining(&mut disposal.extra)?{
                RuntimeDisposalRead::Pending=>return Ok(false),RuntimeDisposalRead::Packet=>anyhow::bail!("SDK disposal caller sent a second packet; retain original"),RuntimeDisposalRead::ReadClosed{..}=>disposal.native_write_eof=true}}
            if disposal.process.is_none(){self.process.as_mut().context("actual original SDK process absent")?.dispose_job_resources_retaining(&mut disposal.process,original,cancel)?;}
            // Original transferred source/out/cancel/reply descriptors are
            // independent aliases from process.helper.fds. Close ALL of them.
            self.packet.fds.clear();original.remaining()?;check_cancel(cancel)?;sdk.current()?;
            if disposal.ack.is_none(){let(pid,start)=disposal.process.as_ref().unwrap().original_helper_data()?;
                disposal.ack=Some(canonical_json(&CloseAck{version:1,kind:"compiler_sdk_job_resources_closed_v1".into(),request:disposal.request.as_ref().unwrap().clone(),original_helper_pid:pid,original_helper_start:start.into()})?);}
            if !disposal.sent{if !self.reply.as_ref().unwrap().send_data(disposal.ack.as_ref().unwrap(),&[])?{return Ok(false)}disposal.sent=true;}
            original.remaining()?;check_cancel(cancel)?;sdk.current()?;owner.proof.current()?;
            // Close the final reply endpoint only after actual resource
            // settlement and known send. Native independently requires FULL
            // peer hangup, not this packet/half-close as a capacity receipt.
            self.reply.take();original.remaining()?;check_cancel(cancel)?;
            disposal.packet.as_mut().unwrap().fds.clear();disposal.ready=true;Ok(true)
        })();
        if let Err(error)=result{disposal.error=Some(error);return Ok(false)}result
    }
    pub(super) fn original_binding(&self)->Result<JobBinding>{let r=self.request.as_ref().context("original SDK request absent")?;
        let binding=JobBinding{serial:r.serial,nonce:r.nonce.clone(),engine_instance_id:r.engine_instance_id.clone(),epoch:r.epoch.clone(),config_sha256:r.config_sha256.clone(),source_root:r.source_root.clone(),action_key:r.action_key.clone(),build_constraint:r.constraint.clone()};binding.validate()?;Ok(binding)}
}
