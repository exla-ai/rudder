//! Distinct terminal-HELPER-failure reply. The same original build deadline
//! governs its send/adoption; it cannot renew execution or authorize success.
use super::*;
use super::super::super::sdk_job_protocol::TerminalFailureAck;
impl RootBuildTransfer {
    pub(super) fn poll_terminal_reply_retaining(&mut self,owner:&RuntimeImageMountOwner)->Result<bool>{
        if self.terminal_sent{return self.poll_failed_job_disposal_retaining(owner)}
        if self.terminal_error.is_some()||!self.admitted{return Ok(false)}
        let result=(||->Result<bool>{owner.current()?;let sdk=owner.compiler_sdk.as_ref().context("SDK actual floor absent")?;sdk.current()?;
            let Some(process)=self.process.as_mut()else{return Ok(false)};
            if !process.poll_terminal_failure_retaining()?{return Ok(false)}
            if self.terminal_ack.is_none(){self.terminal_ack=Some(TerminalFailureAck{version:1,kind:"compiler_sdk_helper_terminal_failure_v1".into(),binding:self.original_binding()?,sdk:sdk.data.as_ref().unwrap().clone(),helper:self.process.as_ref().unwrap().terminal_failure_data()?.clone()});}
            let ack=self.terminal_ack.as_ref().unwrap();ack.validate(&self.original_binding()?,sdk.data.as_ref().unwrap())?;
            self.process.as_ref().unwrap().terminal_reply_current()?;check_cancel(&self.packet.fds[2])?;
            if self.terminal_bytes.is_none(){self.terminal_bytes=Some(canonical_json(ack)?);ensure!(self.terminal_bytes.as_ref().unwrap().len()<=16*1024,"SDK helper-failure frame exceeds bound");}
            if !self.reply.as_ref().context("SDK actual original private reply absent")?.send_data(self.terminal_bytes.as_ref().unwrap(),&[])?{return Ok(false)}
            // Sending is one-shot even if its postcheck fails. A late/unknown
            // reply never becomes cleanup permission or an original ACK.
            self.terminal_sent=true;self.process.as_ref().unwrap().terminal_reply_current()?;check_cancel(&self.packet.fds[2])?;sdk.current()?;self.terminal_ready=true;Ok(true)
        })();if let Err(error)=result{self.terminal_error=Some(error);return Ok(false)}result
    }
}
