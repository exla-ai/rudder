//! Authenticated stop on the original storage connection. EOF, a pidfd or
//! waiting a process never creates this receipt. Failed requests stay retained.
use super::*;

#[must_use="retain original service stop ACK with original bootstrap"]
pub struct RootServiceStopped {instance:uuid::Uuid,retirement:uuid::Uuid}
impl RootServiceStopped {
    pub(crate) fn retirement(&self)->uuid::Uuid {self.retirement}
}
struct StopPacket {serial:u64,bytes:Vec<u8>,sent:bool,send_unknown:bool,received:Option<service::Packet>}
pub(super) struct ServiceStop {id:uuid::Uuid,original:FinalizationBudget,packet:Option<StopPacket>,ack:Option<RootServiceStopped>}
enum StopAnswer {Stopped,Pending}
fn authenticated_reply(packet:&service::Packet,serial:u64,retirement:uuid::Uuid,server:u32)->Result<StopAnswer> {
    ensure!(packet.fds.is_empty()&&packet.credentials.pid==server as i32&&packet.credentials.uid==0&&packet.credentials.gid==0,
        "original service stop reply credentials differ");
    let(answer,payload):(Answer,_)=service::decode(&packet.bytes)?;
    ensure!(answer.version==1&&answer.serial==serial&&payload.is_empty(),"original service stop reply order differs");
    match answer.reply {
        Reply::ServiceStopped{retirement:id}if id==retirement=>Ok(StopAnswer::Stopped),
        Reply::ServiceStopPending{retirement:id}if id==retirement=>Ok(StopAnswer::Pending),
        _=>anyhow::bail!("original service stop ACK differs; all original owners retained"),
    }
}
impl ServiceStop {
    pub(super) fn permits_seed_retirement(&self)->bool {self.packet.is_none()&&self.ack.is_none()}
}
impl RootStorageClient {
    /// OFF App. Same actual client, original deadline and pending request after
    /// every error. A known Pending reply permits seed retirement, then polling
    /// the SAME stop intent again. Unknown ACKs never permit another RPC.
    pub fn poll_service_stop_retaining(&mut self,original:&FinalizationBudget)->Result<bool> {
        original.remaining()?;
        if let Some(stop)=&self.service_stop {ensure!(stop.original.constraint()==original.constraint(),"original service stop budget changed");}
        else {
            self.assert_authenticated_bootstrap_current()?;
            ensure!(self.retirement_inflight.is_none()&&self.cow_inflight.is_none()&&self.seed_window_inflight.is_none()
                &&self.private_v2_recovery.is_none()&&self.generic_inflight.is_none(),"original storage operation requires reconciliation before stop");
            self.service_stop=Some(ServiceStop{id:uuid::Uuid::new_v4(),original:original.clone(),packet:None,ack:None});
        }
        if self.service_stop.as_ref().unwrap().ack.is_some(){return Ok(true)}
        self.stop_host_current()?;
        if self.service_stop.as_ref().unwrap().packet.is_none() {
            ensure!(self.retirement_inflight.is_none()&&self.cow_inflight.is_none()&&self.seed_window_inflight.is_none()
                &&self.private_v2_recovery.is_none()&&self.generic_inflight.is_none(),"Root seed operation still requires original reconciliation before stop");
            self.server.live()?;self.image.matches_process(&self.server)?;
            let serial=self.serial.checked_add(1).context("storage stop serial exhausted")?;
            let id=self.service_stop.as_ref().unwrap().id;
            let bytes=service::encode(&Envelope{version:1,serial,constraint:original.constraint(),request:Request::StopService{retirement:id}},&[])?;
            self.service_stop.as_mut().unwrap().packet=Some(StopPacket{serial,bytes,sent:false,send_unknown:false,received:None});self.serial=serial;
        }
        let stop=self.service_stop.as_mut().unwrap();let packet=stop.packet.as_mut().unwrap();stop.original.remaining()?;
        if !packet.sent {
            self.server.live()?;
            ensure!(!packet.send_unknown,"original service stop send uncertain; explicit reconciliation required");packet.send_unknown=true;
            let sent=self.endpoint.send(&packet.bytes,None)?;packet.send_unknown=false;if !sent{return Ok(false)}
            packet.sent=true;
        }
        if packet.received.is_none(){packet.received=self.endpoint.receive()?;}
        let Some(reply)=packet.received.as_ref()else{return Ok(false)};
        // The service may have already returned normally after its actual send.
        // Authenticate retained credentials/serial/instance, never EOF or exit.
        match authenticated_reply(reply,packet.serial,stop.id,self.server.pid)? {
            StopAnswer::Stopped=>{
                stop.ack=Some(RootServiceStopped{instance:self.instance.context("original service authentication absent")?,retirement:stop.id});
                stop.original.remaining()?;Ok(true)
            },
            StopAnswer::Pending=>{stop.packet=None;stop.original.remaining()?;Ok(false)},
        }
    }
    pub fn service_stop_acknowledgment(&self)->Option<&RootServiceStopped>{
        let ack=self.service_stop.as_ref()?.ack.as_ref()?;
        (Some(ack.instance)==self.instance).then_some(ack)
    }
    fn stop_host_current(&self)->Result<()> {
        ensure!(self.own.pid==std::process::id(),"original stop client process changed");
        service::floor(self.own.pid,1500)?;self.own.live()?;
        ensure!(unsafe{libc::prctl(libc::PR_GET_DUMPABLE,0,0,0,0)}==0,"original stop descriptor protection changed");
        self.engine_image.matches_process(&self.own)?;self.image.matches_fd(&self._inherited_image)?;
        ensure!(service::id(&self.mnt)?==service::id(&service::namespace(None,"mnt",libc::CLONE_NEWNS)?)?
            &&service::id(&self.user)?==service::id(&service::namespace(None,"user",libc::CLONE_NEWUSER)?)?
            &&self.boot==service::proc_bytes("/proc/sys/kernel/random/boot_id",64)?,"original stop host namespace/boot changed");Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Wire/data validation only. These tests do not issue a real Root receipt.
    fn packet(serial:u64,reply:Reply)->service::Packet {
        service::Packet{bytes:service::encode(&Answer{version:1,serial,reply},&[]).unwrap(),fds:Vec::new(),
            credentials:libc::ucred{pid:31,uid:0,gid:0}}
    }
    #[test]
    fn service_stop_ack_requires_original_identity_serial_and_operation() {
        let id=uuid::Uuid::new_v4();let mut held=packet(7,Reply::ServiceStopped{retirement:id});
        assert!(matches!(authenticated_reply(&held,7,id,31).unwrap(),StopAnswer::Stopped));
        assert!(authenticated_reply(&held,8,id,31).is_err());
        assert!(authenticated_reply(&held,7,uuid::Uuid::new_v4(),31).is_err());
        assert!(authenticated_reply(&held,7,id,32).is_err());
        held.credentials.uid=1500;assert!(authenticated_reply(&held,7,id,31).is_err());
        held.credentials.uid=0;held.credentials.gid=1500;assert!(authenticated_reply(&held,7,id,31).is_err());
        held.credentials.gid=0;held.fds.push(File::open("/dev/null").unwrap().into());
        assert!(authenticated_reply(&held,7,id,31).is_err());assert_eq!(held.fds.len(),1,"bad reply descriptor stays in original packet");
    }
    #[test]
    fn pending_or_seed_cleanup_never_counts_as_service_stop() {
        let id=uuid::Uuid::new_v4();
        assert!(matches!(authenticated_reply(&packet(1,Reply::ServiceStopPending{retirement:id}),1,id,31).unwrap(),StopAnswer::Pending));
        assert!(authenticated_reply(&packet(1,Reply::SeedRetired{slot:uuid::Uuid::new_v4(),retirement:id}),1,id,31).is_err());
        assert!(authenticated_reply(&packet(1,Reply::Accepted),1,id,31).is_err());
    }
}
