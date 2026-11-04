//! Empty-use retirement on the ORIGINAL authenticated runtime connection.
//! This deliberately refuses any previously enrolled actor or exported backing
//! until that owner's real aggregate stop protocol exists. There is no PID,
//! EOF, lease, tombstone or caller-provided empty-census shortcut.
use super::*;
const REQUEST:&str="runtime_unshared_retirement_v1";
const ACK:&str="runtime_unshared_retired_v1";
#[derive(Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {version:u32,kind:String,nonce:String,engine_instance_id:String,epoch:String,constraint:FinalizationConstraint}
#[derive(Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Ack {version:u32,kind:String,request:Request}
pub(super) struct NativeRetirement {original:FinalizationBudget,request:Request,bytes:Vec<u8>,sent:bool,send_unknown:bool,pending:Option<RuntimePacket>,ready:bool}
pub(super) struct RootRetirement {packet:RuntimePacket,request:Option<Request>,original:Option<FinalizationBudget>,ack:Option<Vec<u8>>,send_unknown:bool,ready:bool}
impl RootRetirement {pub(super) fn capture(packet:RuntimePacket)->Self{Self{packet,request:None,original:None,ack:None,send_unknown:false,ready:false}}}
impl NativeRetirement {
    // Mechanical ACK inspection only; this private helper cannot construct
    // an original Root/runtime owner or discharge any actor census.
    fn poll_ack(&mut self,endpoint:&RuntimeEndpoint,service_pid:u32,cancel:&AtomicBool)->Result<bool> {
        ensure!(self.sent&&!self.send_unknown,"runtime original retirement request outcome uncertain");
        if self.pending.is_none(){match endpoint.receive_disposal_retaining(&mut self.pending)?{
            wire::RuntimeDisposalRead::Pending=>return Ok(false),wire::RuntimeDisposalRead::Packet=>{},wire::RuntimeDisposalRead::ReadClosed{..}=>anyhow::bail!("runtime original Root closed without retirement ACK")}}
        let packet=self.pending.as_ref().unwrap();
        ensure!(packet.credentials.pid==service_pid as i32&&packet.credentials.uid==0&&packet.credentials.gid==0&&packet.fds.is_empty(),"runtime retirement ACK has foreign original service");
        let ack:Ack=serde_json::from_slice(&packet.bytes)?;
        ensure!(ack.version==1&&ack.kind==ACK&&ack.request==self.request,"runtime original retirement ACK differs");
        time(&self.original,cancel)?;self.ready=true;Ok(true)
    }
}
pub(super) fn is_request(bytes:&[u8])->Result<bool>{let data:serde_json::Value=serde_json::from_slice(bytes)?;Ok(data["kind"]==REQUEST)}
fn time(original:&FinalizationBudget,cancel:&AtomicBool)->Result<()> {
    ensure!(!cancel.load(Ordering::Acquire),"runtime retirement cancelled; original owners retained");original.remaining()?;Ok(())
}
impl RuntimeImageClient {
    /// OFF App. The same runtime client and original cleanup clock survive
    /// Pending/errors. Only an empty, absorbingly closed original use ledger
    /// can enter this restricted workflow. Existing uses remain quarantined.
    pub fn poll_unshared_retirement_retaining(&self,original:&FinalizationBudget,cancel:&AtomicBool)->Result<bool> {
        time(original,cancel)?;self.current()?;self.close_original_use_admission()?;
        if !self.uses.closed_and_empty()?{return Ok(false)}
        {let sdk=self.compiler_sdk.try_lock().map_err(|_|anyhow::anyhow!("SDK retirement owner busy/poisoned"))?;
            if let Some(sdk)=&*sdk{if !sdk.builds_retired()?{return Ok(false)}}}
        let mut state=match self.state.try_lock(){Ok(s)=>s,Err(TryLockError::WouldBlock)=>return Ok(false),Err(_)=>anyhow::bail!("runtime retirement carrier poisoned")};
        ensure!(state.active.is_none()&&state.retained.is_empty()&&state.backing_active.is_none()&&state.backing_retained.is_none(),"runtime original use census disagrees; retain all owners");
        if state.retirement.is_none(){
            let registration=self.registration.data();
            let request=Request{version:1,kind:REQUEST.into(),nonce:uuid::Uuid::new_v4().to_string(),engine_instance_id:registration.engine_instance_id.clone(),epoch:registration.binding.epoch.clone(),constraint:original.constraint()};
            let bytes=canonical_json(&request)?;
            state.retirement=Some(NativeRetirement{original:original.clone(),request,bytes,sent:false,send_unknown:false,pending:None,ready:false});
        }
        let ClientState{endpoint,retirement,..}=&mut *state;let retained=retirement.as_mut().unwrap();
        ensure!(retained.original.constraint()==original.constraint(),"runtime retirement original clock replaced");
        if retained.ready{return Ok(true)}
        time(original,cancel)?;
        if !retained.sent{
            ensure!(!retained.send_unknown,"runtime original retirement send uncertain; explicit reconciliation required");retained.send_unknown=true;
            let sent=endpoint.send_data(&retained.bytes,&[])?;retained.send_unknown=false;if !sent{return Ok(false)}retained.sent=true;
        }
        retained.poll_ack(endpoint,self.service.pid,cancel)
    }
}

#[cfg(test)]
#[path="root_runtime_retirement_tests.rs"]
mod tests;
impl RootRuntimeService {
    pub(super) fn poll_original_retirement(&mut self)->Result<bool> {
        let retained=self.retirement.as_mut().context("runtime original retirement carrier absent")?;
        if retained.ready{return Ok(false)}
        let proof=self.proof.as_ref().context("runtime original bootstrap proof absent")?;proof.current()?;
        if retained.request.is_none(){
            let packet=&retained.packet;
            ensure!(packet.credentials.pid==proof.engine.pid as i32&&packet.credentials.uid==1500&&packet.credentials.gid==1500&&packet.fds.is_empty(),"runtime retirement request is not original Native owner");
            let request:Request=serde_json::from_slice(&packet.bytes)?;
            let owner=self.owner.as_ref().context("runtime retirement original floor absent")?;
            let registration=&owner.admission.as_ref().context("runtime retirement actual registration absent")?.registration.ack;
            ensure!(request.version==1&&request.kind==REQUEST&&uuid::Uuid::parse_str(&request.nonce).is_ok_and(|id|id.to_string()==request.nonce)
                &&request.engine_instance_id==registration.engine_instance_id&&request.epoch==registration.binding.epoch,"runtime retirement original binding differs");
            retained.original=Some(FinalizationBudget::from_constraint(&request.constraint)?);retained.request=Some(request);
        }
        let original=retained.original.as_ref().unwrap();original.remaining()?;
        // A grant or a half-completed export remains an actual actor owner.
        // No aggregate count or process observation can discharge these.
        if !self.sessions.is_empty()||self.backing_transfer.is_some()||!self.compiler_jobs.is_empty()||self.pending.is_some(){return Ok(false)}
        let owner=self.owner.as_mut().unwrap();
        if let Some(sdk)=&mut owner.compiler_sdk{if !sdk.retire_unshared_retaining(original,&AtomicBool::new(false))?{return Ok(false)}}
        owner.close_admission();owner.admission.as_ref().unwrap().reads_closed.store(true,Ordering::Release);
        if let Some(floor)=&owner.floor{floor.close_admission();}
        owner.components.clear();owner.root_fd.take();
        let mount=owner.mount.as_mut().context("runtime original FUSE mount absent")?;
        if !mount.unmount_and_checkpoint_retaining(original,&AtomicBool::new(false))?{return Ok(false)}
        ensure!(mount.is_unmounted_and_joined(),"runtime original FUSE worker/TLS join absent");
        if let Some(read)=&owner.manifest_read{read.verified_bytes()?;} // Original synchronous manifest read completed.
        if owner.pending_file.is_some()||owner.floor.as_ref().context("runtime original read floor absent")?.retained_requests_data()?!=0{return Ok(false)}
        original.remaining()?;proof.current()?;
        if retained.ack.is_none(){retained.ack=Some(canonical_json(&Ack{version:1,kind:ACK.into(),request:retained.request.as_ref().unwrap().clone()})?);}
        ensure!(!retained.send_unknown,"runtime original retirement ACK uncertain; explicit reconciliation required");retained.send_unknown=true;
        let sent=self.engine.as_ref().unwrap().send_data(retained.ack.as_ref().unwrap(),&[])?;
        retained.send_unknown=false;if !sent{return Ok(false)}
        retained.ready=true;Ok(true)
    }
    /// Called only from the actual storage stop owner. Its request follows
    /// Native's original runtime ACK. Re-reading this receipt cannot retire an
    /// enrolled runtime, replace a clock or manufacture any missing joins.
    pub(crate) fn retire_unshared_resources_retaining(&mut self,original:&FinalizationBudget,cancel:&AtomicBool)->Result<bool> {
        time(original,cancel)?;
        Ok(self.retirement.as_ref().is_some_and(|retained|retained.ready&&retained.original.as_ref().is_some_and(|held|held.constraint()==original.constraint())))
    }
}
