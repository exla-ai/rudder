//! Private generation adoption on the SAME original Root/native endpoint.
//! Serialized fields alone grant nothing; every packet must be received with
//! original Root SCM credentials and actual pidfd/image/namespace descriptors.
use super::*;
#[derive(Clone,Serialize,Deserialize)]#[serde(deny_unknown_fields)]
pub(super) struct NativeOffer {pub version:u32,pub kind:String,pub serial:String,pub generation:String,pub attempt_nonce:String,pub config_sha256:String,pub root_nonce:String,pub constraint:FinalizationConstraint,pub label:String}
#[derive(Clone,Serialize,Deserialize)]#[serde(deny_unknown_fields)]
pub(super) struct NativeAccepted {pub version:u32,pub kind:String,pub serial:String,pub generation:String,pub attempt_nonce:String,pub config_sha256:String,pub constraint:FinalizationConstraint}
#[derive(Clone,Serialize,Deserialize)]#[serde(deny_unknown_fields)]
pub(super) struct NativeState {pub version:u32,pub kind:String,pub serial:String,pub generation:String,pub attempt_nonce:String,pub config_sha256:String}
struct Staged {offer:NativeOffer,packets:Vec<storage::Packet>,witness:Option<SidecarWitness>,ack:Option<Vec<u8>>,ack_sent:bool,expired:bool}
pub(super) struct NativeTransport {serial:u64,generation:String,online:bool,staged:Option<Staged>,retired:Vec<Staged>,received:Option<storage::Packet>,historical:Vec<storage::Packet>}
impl NativeTransport {
 pub(super) fn initial(generation:String)->Self {Self {serial:0,generation,online:false,staged:None,retired:Vec::new(),received:None,historical:Vec::new()}}
 pub(super) fn online(&self)->bool {self.online}
 pub(super) fn generation(&self)->&str {&self.generation}
 fn candidate_current(staged:&mut Staged)->Result<bool>{
  if FinalizationBudget::from_constraint(&staged.offer.constraint).and_then(|b|b.remaining()).is_err(){staged.expired=true;return Ok(false);}
  let witness=staged.witness.as_ref().context("actual sidecar witness absent")?;
  if witness.process.exited()?{staged.expired=true;return Ok(false);}
  if let Err(error)=witness.check_transferred(){if witness.process.exited()?{staged.expired=true;return Ok(false);}return Err(error);}
  if FinalizationBudget::from_constraint(&staged.offer.constraint).and_then(|b|b.remaining()).is_err(){staged.expired=true;return Ok(false);}Ok(true)
 }
 fn serial(value:&str)->Result<u64>{ensure!(!value.is_empty()&&!value.starts_with('0')&&value.bytes().all(|b|b.is_ascii_digit())&&value.len()<=19,"host transport serial invalid");let n=value.parse::<u64>()?;ensure!(n<=i64::MAX as u64,"host transport serial exhausted");Ok(n)}
 fn root(config:&HostIngressConfigOwner,p:&storage::Packet)->Result<()> {config.assert_current()?;ensure!(p.credentials.pid==config.root.pid as i32&&p.credentials.uid==0&&p.credentials.gid==0&&p.bytes.len()<=MAX_HOST_FRAME,"transport packet is not original Root");Ok(())}
 pub(super) fn is_control(packet:&storage::Packet)->bool {serde_json::from_slice::<serde_json::Value>(&packet.bytes).ok().and_then(|v|v.get("kind").and_then(|s|s.as_str()).map(|s|s.starts_with("host_transport_"))).unwrap_or(false)}
 /// Stage actual packet/FDs BEFORE encoding, identity, time or witness checks.
 pub(super) fn consume_retaining(&mut self,packet:storage::Packet,config:&HostIngressConfigOwner,endpoint:&Endpoint)->Result<()> {
  ensure!(self.received.is_none(),"original unknown generation packet retained");self.received=Some(packet);let p=self.received.as_ref().unwrap();Self::root(config,p)?;
  let kind:serde_json::Value=serde_json::from_slice(&p.bytes)?;
  if kind["kind"]=="host_transport_offer_v1" {
   let offer:NativeOffer=serde_json::from_slice(&p.bytes)?;let n=Self::serial(&offer.serial)?;uuid(&offer.generation)?;uuid(&offer.attempt_nonce)?;
   ensure!(offer.version==1&&offer.config_sha256==config.digest&&offer.root_nonce==config.bootstrap_nonce&&p.fds.len()==1&&n>self.serial,"transport offer binding differs");
   FinalizationConstraint::from_json(&canonical_json(&offer.constraint)?)?;
   ensure!(serde_json::to_value(&offer.constraint)?["maximum_milliseconds"].as_u64().is_some_and(|n|(1..=5000).contains(&n)),"transport offer clock exceeds bound");
   if self.staged.as_ref().is_some_and(|old|old.offer.attempt_nonce!=offer.attempt_nonce){let known=self.staged.as_ref().is_some_and(|old|old.ack_sent&&Self::serial(&old.offer.serial).ok()==Some(self.serial));
    if known {self.staged.take();}else{ensure!(self.retired.len()<4,"unknown native transport offers retained at bound");self.retired.push(self.staged.take().unwrap());}}
   if self.staged.is_none(){self.staged=Some(Staged {offer:offer.clone(),packets:Vec::new(),witness:None,ack:None,ack_sent:false,expired:false});}
   let staged=self.staged.as_mut().unwrap();let index=staged.packets.len();let labels=["sidecar","exe","mnt","user","net","pid"];
   ensure!(index<labels.len()&&offer.label==labels[index]&&offer.serial==staged.offer.serial&&offer.generation==staged.offer.generation&&offer.attempt_nonce==staged.offer.attempt_nonce&&offer.constraint==staged.offer.constraint,"transport offer descriptor order/identity differs");
   staged.packets.push(self.received.take().unwrap());if FinalizationBudget::from_constraint(&offer.constraint).and_then(|b|b.remaining()).is_err(){staged.expired=true;return Ok(());}
   if staged.packets.len()==labels.len(){let process=Process::capture(staged.packets[0].fds[0].try_clone()?)?;let exe=staged.packets[1].fds[0].try_clone()?;
    staged.witness=Some(SidecarWitness {process,image:Image::capture(Path::new(NODE))?,exe,namespaces:labels[2..].iter().zip([libc::CLONE_NEWNS,libc::CLONE_NEWUSER,libc::CLONE_NEWNET,libc::CLONE_NEWPID]).enumerate().map(|(i,(name,kind))|Ok(((*name).into(),File::from(staged.packets[i+2].fds[0].try_clone()?),kind))).collect::<Result<_>>()?});
    if !Self::candidate_current(staged)?{return Ok(());}
    let ack=NativeAccepted {version:1,kind:"host_transport_native_accepted_v1".into(),serial:offer.serial.clone(),generation:offer.generation.clone(),attempt_nonce:offer.attempt_nonce.clone(),config_sha256:config.digest.clone(),constraint:offer.constraint.clone()};staged.ack=Some(canonical_json(&ack)?);
   }
   return self.flush_acceptance(endpoint,config);
  }
  ensure!(p.fds.is_empty(),"transport state carries unexpected authority");let state:NativeState=serde_json::from_slice(&p.bytes)?;ensure!(state.version==1&&state.config_sha256==config.digest,"transport state config differs");let serial=Self::serial(&state.serial)?;
  if state.kind=="host_transport_offline_v1" {ensure!(serial>=self.serial,"stale transport offline notice");self.online=false;self.received.take();return Ok(());}
  ensure!(state.kind=="host_transport_active_v1","transport state kind invalid");let staged=self.staged.as_mut().context("actual staged generation witness absent")?;
  ensure!(staged.ack_sent&&staged.offer.serial==state.serial&&staged.offer.generation==state.generation&&staged.offer.attempt_nonce==state.attempt_nonce,"transport activation lacks actual original acceptance");
  if !Self::candidate_current(staged)?{ensure!(self.historical.len()<8,"late transport packets retained at bound");self.historical.push(self.received.take().unwrap());return Ok(());}config.assert_current()?;
  self.serial=serial;self.generation=state.generation;self.online=true;self.received.take();Ok(())
 }
 pub(super) fn flush_acceptance(&mut self,endpoint:&Endpoint,config:&HostIngressConfigOwner)->Result<()> {if let Some(staged)=&mut self.staged {if !staged.ack_sent&&!staged.expired&&staged.ack.is_some(){if !Self::candidate_current(staged)?{return Ok(());}config.assert_current()?;if endpoint.send(staged.ack.as_ref().unwrap(),None)?{staged.ack_sent=true;}}}Ok(())}
}

#[cfg(test)]
#[path="host_transport_native_tests.rs"]
mod tests;
