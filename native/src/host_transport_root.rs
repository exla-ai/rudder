//! Actual original Root registration is the only caller/signing owner.
//! Replaces transport generation, never engine identity, source authority,
//! operation clocks, lease epoch, or an unknown effect.
use super::*;
use std::time::Duration;
const BEGIN:&[u8]=b"rudder_host_transport_begin_v1\0";
const CHALLENGE:&[u8]=b"rudder_host_transport_challenge_v1\0";
const COMMIT:&[u8]=b"rudder_host_transport_commit_v1\0";
const ACK:&[u8]=b"rudder_host_transport_ack_v1\0";
const CONFIRM:&[u8]=b"rudder_host_transport_confirm_v1\0";
const RECONCILE:&[u8]=b"rudder_host_transport_reconcile_v1\0";
#[derive(Clone,Serialize,Deserialize)]#[serde(deny_unknown_fields)]
struct Begin {version:u32,kind:String,registration:RegistrationAck,serial:String,generation:String,attempt_nonce:String,constraint:FinalizationConstraint,remaining_milliseconds:u32}
#[derive(Clone,Serialize,Deserialize)]#[serde(deny_unknown_fields)]
struct Challenge {version:u32,kind:String,registration:RegistrationAck,serial:String,generation:String,attempt_nonce:String,constraint:FinalizationConstraint,challenge_nonce:String,begin_sha256:String}
#[derive(Clone,Serialize,Deserialize)]#[serde(deny_unknown_fields)]
struct Commit {version:u32,kind:String,registration:RegistrationAck,serial:String,generation:String,attempt_nonce:String,constraint:FinalizationConstraint,challenge_nonce:String,begin_sha256:String,native_acceptance_sha256:String}
#[derive(Clone,Serialize,Deserialize)]#[serde(deny_unknown_fields)]
struct Ack {version:u32,kind:String,registration:RegistrationAck,serial:String,generation:String,attempt_nonce:String,constraint:FinalizationConstraint,challenge_nonce:String,begin_sha256:String,native_acceptance_sha256:String,commit_sha256:String}
#[derive(Clone,Copy,PartialEq)]enum Phase {Connect,Ready,Begin,Challenge,NativeOffer,NativeAck,Commit,Ack,NativeActive,SidecarActive,Serving,Unknown}
struct Attempt {original:FinalizationBudget,serial:String,generation:String,nonce:String,phase:Phase,begin:Option<Begin>,wire:Option<Vec<u8>>,inbound:Option<storage::Packet>,challenge:Option<Signed<Challenge>>,native_ack:Option<storage::Packet>,commit:Option<Commit>,ack:Option<Signed<Ack>>,unknown:Option<anyhow::Error>,bookkeeping:Option<Vec<Vec<u8>>>,bookkeeping_at:usize,bookkeeping_unknown:Option<anyhow::Error>,inspect_last:Option<std::time::Instant>}
pub(super) struct HostTransportOwner {serial:u64,attempt:Option<Attempt>,retired:Vec<Attempt>}
impl HostTransportOwner {
 pub(super) fn new()->Self {Self {serial:0,attempt:None,retired:Vec::new()}}
 pub(super) fn poll_retaining(&mut self,host:&mut RootHostControl,registration:&RegistrationAck,credential:&[u8],engine:&Process,_initial:&FinalizationBudget,pending:&[serde_json::Value])->Result<()> {
  engine.live()?;host.check_transport_policy_current()?;
  let lost=host.poll_transport_health_retaining(engine)?;
  if self.attempt.as_ref().is_some_and(|a|a.phase==Phase::Serving)&&!lost{let a=self.attempt.as_mut().unwrap();
   // Historical receipt acknowledgement/readback cannot gate an otherwise
   // authenticated healthy generation and cannot create a fresh lease.
   if a.bookkeeping_unknown.is_none(){let result=(||->Result<()>{if a.bookkeeping.as_ref().is_some_and(|packets|a.bookkeeping_at==packets.len())&&!pending.is_empty()&&a.inspect_last.is_some_and(|last|last.elapsed()>=Duration::from_millis(500)){
     // Re-read only unchanged original ACK DATA if the original effect was
     // still running during the first inspection. Never call an effect again.
     let reconcile=serde_json::json!({"version":1,"kind":"host_transport_reconcile","registration":registration,"serial":a.serial,"generation":a.generation,"attempt_nonce":a.nonce,"requests":pending});
     a.bookkeeping=Some(vec![bytes(&Relay {kind:"host_transport_reconcile_v1".into(),rpc_id:a.nonce.clone(),packet:Signed {mac:sign(credential,RECONCILE,&reconcile)?,body:reconcile}})?]);a.bookkeeping_at=0;
    }
    if a.bookkeeping.is_none(){let ack=&a.ack.as_ref().unwrap().body;
     let confirm=serde_json::json!({"version":1,"kind":"host_transport_confirm","registration":registration,"serial":a.serial,"generation":a.generation,"attempt_nonce":a.nonce,"commit_sha256":ack.commit_sha256});
     let reconcile=serde_json::json!({"version":1,"kind":"host_transport_reconcile","registration":registration,"serial":a.serial,"generation":a.generation,"attempt_nonce":a.nonce,"requests":pending});
     a.bookkeeping=Some(vec![bytes(&Relay {kind:"host_transport_confirm_v1".into(),rpc_id:a.nonce.clone(),packet:Signed {mac:sign(credential,CONFIRM,&confirm)?,body:confirm}})?,bytes(&Relay {kind:"host_transport_reconcile_v1".into(),rpc_id:a.nonce.clone(),packet:Signed {mac:sign(credential,RECONCILE,&reconcile)?,body:reconcile}})?]);}
    let packets=a.bookkeeping.as_ref().unwrap();if a.bookkeeping_at<packets.len()&&host.transport_send_retaining(&packets[a.bookkeeping_at])?{a.bookkeeping_at+=1;if a.bookkeeping_at==packets.len(){a.inspect_last=Some(std::time::Instant::now());}}Ok(())})();if let Err(error)=result{a.bookkeeping_unknown=Some(error);}}
   return Ok(());}
  if self.attempt.as_ref().is_some_and(|a|a.phase==Phase::Serving)&&lost {host.pause_native_transport_retaining()?;let old=self.attempt.take().unwrap();self.retired.push(old);}
  // Actual known serving generations contain no mutation owners. Only their
  // duplicate DATA receipts can be recycled; unknown attempts remain held.
  self.retired.retain(|a|a.phase!=Phase::Serving);ensure!(self.retired.len()<8,"unknown Root transport attempts retained at bound");
  if self.attempt.is_none(){self.serial=self.serial.checked_add(1).context("transport serial exhausted")?;ensure!(self.serial<=i64::MAX as u64,"transport serial exhausted");
   let duration=Duration::from_secs(5);
   // BEFORE queueing/connect/spawn/TLS. This fresh transport clock does not
   // touch any original30/15/300 mutation or lease clock.
   self.attempt=Some(Attempt {original:FinalizationBudget::begin(duration)?,serial:self.serial.to_string(),generation:uuid::Uuid::new_v4().to_string(),nonce:uuid::Uuid::new_v4().to_string(),phase:Phase::Connect,begin:None,wire:None,inbound:None,challenge:None,native_ack:None,commit:None,ack:None,unknown:None,bookkeeping:None,bookkeeping_at:0,bookkeeping_unknown:None,inspect_last:None});
  }
  if self.attempt.as_ref().is_some_and(|a|a.phase==Phase::Unknown){let a=self.attempt.as_ref().unwrap();
   // Cancelling an old socket is not settlement. Root may start a distinct
   // auth-only generation only after actual original socket/process closure;
   // every unknown packet/constraint/effect owner stays in retired custody.
   if !host.close_unknown_transport_retaining(&a.serial,&a.generation,&a.nonce)?{return Ok(());}self.retired.push(self.attempt.take().unwrap());return Ok(());}
  let a=self.attempt.as_mut().unwrap();
  let result=(||->Result<()>{a.original.remaining()?;engine.live()?;host.check_transport_policy_current()?;
   match a.phase {
    Phase::Connect=>{if host.connect_transport_generation_retaining(&a.serial,&a.generation,&a.nonce,&a.original)?{a.phase=Phase::Ready;}},
    Phase::Ready=>{if host.transport_generation_ready(&a.serial,&a.generation,&a.nonce)? {host.check_actual_sidecar_for_generation()?;a.phase=Phase::Begin;}},
    Phase::Begin=>{let b=Begin {version:1,kind:"host_transport_begin".into(),registration:registration.clone(),serial:a.serial.clone(),generation:a.generation.clone(),attempt_nonce:a.nonce.clone(),constraint:a.original.constraint(),remaining_milliseconds:u32::try_from(a.original.remaining()?.as_millis().min(5000))?};ensure!(b.remaining_milliseconds>0,"transport original budget lost");let signed=Signed {mac:sign(credential,BEGIN,&b)?,body:b.clone()};a.wire=Some(bytes(&Relay {kind:"host_transport_begin_v1".into(),rpc_id:a.nonce.clone(),packet:signed})?);a.begin=Some(b);
     if host.transport_send_retaining(a.wire.as_ref().unwrap())?{a.phase=Phase::Challenge;}},
    Phase::Challenge=>{host.transport_receive_retaining(&mut a.inbound)?;if let Some(p)=&a.inbound {let relay:Relay<Signed<Challenge>>=serde_json::from_slice(&p.bytes)?;ensure!(relay.kind=="host_transport_challenge_v1"&&relay.rpc_id==a.nonce,"foreign transport challenge");verify(credential,CHALLENGE,&relay.packet)?;let c=&relay.packet.body;let b=a.begin.as_ref().unwrap();
      ensure!(c.version==1&&c.kind=="host_transport_challenge"&&c.registration==*registration&&c.serial==a.serial&&c.generation==a.generation&&c.attempt_nonce==a.nonce&&c.constraint==a.original.constraint()&&c.begin_sha256==hex(&Sha256::digest(bytes(b)?)),"transport challenge differs from original attempt");unhex::<32>(c.challenge_nonce.as_bytes())?;a.challenge=Some(relay.packet);a.phase=Phase::NativeOffer;}},
    Phase::NativeOffer=>{if host.offer_native_generation_retaining(&a.serial,&a.generation,&a.nonce,&a.original)?{a.phase=Phase::NativeAck;}},
    Phase::NativeAck=>{host.native_generation_ack_retaining(&mut a.native_ack,engine)?;if let Some(p)=&a.native_ack {let value:serde_json::Value=serde_json::from_slice(&p.bytes)?;ensure!(value["kind"]=="host_transport_native_accepted_v1"&&value["serial"]==a.serial&&value["generation"]==a.generation&&value["attempt_nonce"]==a.nonce&&value["config_sha256"]==host.registration_config_sha()&&value["constraint"]==serde_json::to_value(a.original.constraint())?,"actual native generation ACK differs");
      let c=&a.challenge.as_ref().unwrap().body;a.commit=Some(Commit {version:1,kind:"host_transport_commit".into(),registration:registration.clone(),serial:a.serial.clone(),generation:a.generation.clone(),attempt_nonce:a.nonce.clone(),constraint:a.original.constraint(),challenge_nonce:c.challenge_nonce.clone(),begin_sha256:c.begin_sha256.clone(),native_acceptance_sha256:hex(&Sha256::digest(canonical_json(&value)?))});let c=a.commit.as_ref().unwrap();a.wire=Some(bytes(&Relay {kind:"host_transport_commit_v1".into(),rpc_id:a.nonce.clone(),packet:Signed {mac:sign(credential,COMMIT,c)?,body:c}})?);a.phase=Phase::Commit;}},
    Phase::Commit=>{if host.transport_send_retaining(a.wire.as_ref().unwrap())?{a.phase=Phase::Ack;}},
    Phase::Ack=>{a.inbound.take();host.transport_receive_retaining(&mut a.inbound)?;if let Some(p)=&a.inbound {let relay:Relay<Signed<Ack>>=serde_json::from_slice(&p.bytes)?;ensure!(relay.kind=="host_transport_ack_v1"&&relay.rpc_id==a.nonce,"foreign transport ACK");verify(credential,ACK,&relay.packet)?;let ack=&relay.packet.body;let c=a.commit.as_ref().unwrap();
      ensure!(ack.version==1&&ack.kind=="host_transport_ack"&&ack.registration==*registration&&ack.serial==c.serial&&ack.generation==c.generation&&ack.attempt_nonce==c.attempt_nonce&&ack.constraint==c.constraint&&ack.challenge_nonce==c.challenge_nonce&&ack.begin_sha256==c.begin_sha256&&ack.native_acceptance_sha256==c.native_acceptance_sha256&&ack.commit_sha256==hex(&Sha256::digest(bytes(c)?)),"transport ACK differs from original native acceptance/commit");a.ack=Some(relay.packet);a.phase=Phase::NativeActive;}},
    Phase::NativeActive=>{if host.activate_native_generation_retaining(&a.serial,&a.generation,&a.nonce,&a.original)?{a.phase=Phase::SidecarActive;}},
    Phase::SidecarActive=>{if host.activate_sidecar_generation_retaining(&a.serial,&a.generation,&a.nonce,&a.original)?{a.phase=Phase::Serving;}},
    Phase::Serving|Phase::Unknown=>{},
   }a.original.remaining()?;engine.live()?;host.check_transport_policy_current()?;Ok(())})();
  if let Err(error)=result {a.unknown=Some(error);a.phase=Phase::Unknown;host.pause_native_transport_retaining()?;}
  Ok(())
 }
}
