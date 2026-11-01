//! Private job-disposal DATA. Constructors below do not grant SDK/source/Root
//! authority. Issuers are the actual retained Native and Root job owners.
use super::*;
use rudder_engine::protocol::{ObjectKind,ObjectRef};

#[derive(Clone,Debug,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct JobBinding {pub serial:u64,pub nonce:String,pub engine_instance_id:String,pub epoch:String,pub config_sha256:String,
    pub source_root:ObjectRef,pub action_key:ObjectRef,pub build_constraint:FinalizationConstraint}
impl JobBinding {
    pub(super) fn validate(&self)->Result<()>{ensure!(self.serial>0&&self.nonce==dispatch_nonce(&self.engine_instance_id,&self.epoch,&self.config_sha256,self.serial)?,"SDK job serial/nonce binding differs");
        ensure!(!self.engine_instance_id.is_empty()&&!self.epoch.is_empty()&&self.config_sha256.len()==64,"SDK job original scope absent");
        self.source_root.validate()?;self.action_key.validate()?;ensure!(self.source_root.kind==ObjectKind::SourceRoot&&self.action_key.kind==ObjectKind::ActionKey,"SDK job typed binding differs");Ok(())}
}
/// A monotonic serial belongs to actual transport SEND order. A known EAGAIN
/// packet has NO effect and may be assigned a later serial under the SAME job
/// and build clock. Unknown/sent packets never change identity or retry.
pub(super) fn dispatch_nonce(engine:&str,epoch:&str,config:&str,serial:u64)->Result<String>{ensure!(serial>0,"SDK dispatch serial absent");
    let raw=Sha256::digest(canonical_json(&serde_json::json!({"schema":"original-sdk-dispatch-v1","engine":engine,"epoch":epoch,"config":config,"serial":serial}))?);
    let mut bytes=[0u8;16];bytes.copy_from_slice(&raw[..16]);bytes[6]=(bytes[6]&0x0f)|0x50;bytes[8]=(bytes[8]&0x3f)|0x80;Ok(uuid::Uuid::from_bytes(bytes).to_string())}
/// Replay bookkeeping only; the Root producer MUST authenticate the actual
/// original packet/engine/epoch before calling this DATA helper.
pub(super) fn admit_serial_data(binding:&JobBinding,high_watermark:&mut u64)->Result<()>{binding.validate()?;ensure!(binding.serial>*high_watermark,"SDK original authenticated serial replay refused");*high_watermark=binding.serial;Ok(())}

#[derive(Clone,Debug,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CloseRequest {pub version:u32,pub kind:String,pub binding:JobBinding,pub artifact_result:ObjectRef,pub cleanup_constraint:FinalizationConstraint}
impl CloseRequest {pub(super) fn validate(&self)->Result<()>{ensure!(self.version==1&&self.kind=="compiler_sdk_job_close_v1","SDK disposal request format differs");self.binding.validate()?;self.artifact_result.validate()?;
    ensure!(self.artifact_result.kind==ObjectKind::ActionResult,"SDK disposal lacks actual artifact publication identity");Ok(())}}
#[derive(Clone,Debug,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CloseAck {pub version:u32,pub kind:String,pub request:CloseRequest,pub original_helper_pid:u32,pub original_helper_start:String}
impl CloseAck {pub(super) fn validate(&self,expected:&CloseRequest)->Result<()>{self.request.validate()?;ensure!(self.version==1&&self.kind=="compiler_sdk_job_resources_closed_v1"&&&self.request==expected
    &&self.original_helper_pid>1&&!self.original_helper_start.is_empty(),"SDK original disposal ACK differs");Ok(())}}

pub(super) fn time(original:&FinalizationBudget,cancel:&AtomicBool)->Result<()>{if cancel.load(Ordering::Acquire){original.cancel();}original.remaining()?;Ok(())}

#[cfg(test)]mod tests {use super::*;
    fn request(serial:u64)->CloseRequest{let original=FinalizationBudget::begin(Duration::from_secs(5)).unwrap();let config="a".repeat(64);CloseRequest{version:1,kind:"compiler_sdk_job_close_v1".into(),
        binding:JobBinding{serial,nonce:dispatch_nonce("engine","epoch",&config,serial).unwrap(),engine_instance_id:"engine".into(),epoch:"epoch".into(),config_sha256:config,
            source_root:ObjectRef::from_bytes(ObjectKind::SourceRoot,b"DATA source"),action_key:ObjectRef::from_bytes(ObjectKind::ActionKey,b"DATA action"),build_constraint:original.constraint()},
        artifact_result:ObjectRef::from_bytes(ObjectKind::ActionResult,b"DATA artifact"),cleanup_constraint:FinalizationBudget::begin(Duration::from_secs(5)).unwrap().constraint()}}
    #[test]fn original_dispatch_nonce_binds_scope_and_monotonic_serial_without_semantic_key_input(){let a=dispatch_nonce("original","epoch","config",1).unwrap();
        assert_ne!(a,dispatch_nonce("original","epoch","config",2).unwrap());assert_ne!(a,dispatch_nonce("other","epoch","config",1).unwrap());assert_ne!(a,dispatch_nonce("original","next","config",1).unwrap());
        assert_eq!(uuid::Uuid::parse_str(&a).unwrap().to_string(),a);assert!(dispatch_nonce("original","epoch","config",0).is_err());}
    #[test]fn sdk_disposed_entry_replay_stays_refused_without_retaining_every_old_job(){let mut high=0;let old=request(1);admit_serial_data(&old.binding,&mut high).unwrap();
        // Original physical job entry may be removed; high-watermark survives.
        assert!(admit_serial_data(&old.binding,&mut high).is_err());let mut renumbered=old.binding.clone();renumbered.serial=2;assert!(admit_serial_data(&renumbered,&mut high).is_err());assert_eq!(high,1);
        let next=request(9);admit_serial_data(&next.binding,&mut high).unwrap();assert_eq!(high,9);assert!(admit_serial_data(&old.binding,&mut high).is_err());}
    #[test]fn sdk_closure_data_binds_same_job_artifact_and_cleanup_clock_without_renewing_build(){let original=request(1);let ack=CloseAck{version:1,kind:"compiler_sdk_job_resources_closed_v1".into(),request:original.clone(),original_helper_pid:42,original_helper_start:"123".into()};ack.validate(&original).unwrap();
        let mut changed=original.clone();changed.artifact_result=ObjectRef::from_bytes(ObjectKind::ActionResult,b"other actual artifact");assert!(ack.validate(&changed).is_err());
        changed=original.clone();changed.cleanup_constraint=FinalizationBudget::begin(Duration::from_secs(2)).unwrap().constraint();assert!(ack.validate(&changed).is_err());
        changed=original.clone();changed.binding.epoch="other".into();assert!(ack.validate(&changed).is_err());}
}

/// Actual original HELPER exit/pipe DATA, never a compiler exit or a successful
/// build/publication/cache receipt. Its opaque issuer owns the original Child.
#[derive(Clone,Debug,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct HelperFailureData {
    pub source_kind:String,pub original_helper_pid:u32,pub original_helper_start:String,
    pub exit_code:Option<i32>,pub signal:Option<i32>,pub stdout_eof:bool,pub stderr_eof:bool,
    pub stdout_bytes:u64,pub stderr_bytes:u64,pub stdout_sha256:String,pub stderr_sha256:String,pub stderr_diagnostic:String,
}
impl HelperFailureData {pub(super) fn validate(&self)->Result<()>{
    ensure!(self.source_kind=="helper_failure"&&self.original_helper_pid>1&&!self.original_helper_start.is_empty()&&self.original_helper_start.len()<=128,"SDK helper-failure identity differs");
    ensure!(matches!((self.exit_code,self.signal),(Some(1..=255),None)|(None,Some(1..=127)))&&self.stdout_eof&&self.stderr_eof,"SDK helper-failure lacks actual nonzero exit and both EOF");
    fn sha(value:&str)->bool{value.len()==64&&value.bytes().all(|b|b.is_ascii_digit()||(b'a'..=b'f').contains(&b))}
    ensure!(self.stdout_bytes<=65536&&self.stderr_bytes<=65536&&sha(&self.stdout_sha256)&&sha(&self.stderr_sha256)&&self.stderr_diagnostic.len()<=4096,"SDK helper-failure diagnostic bounds differ");Ok(())
}}
#[derive(Clone,Debug,PartialEq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TerminalFailureAck {pub version:u32,pub kind:String,pub binding:JobBinding,pub sdk:rudder_engine::execution::compiler_sdk::CompilerSdkMountData,pub helper:HelperFailureData}
impl TerminalFailureAck {pub(super) fn validate(&self,expected:&JobBinding,mount:&rudder_engine::execution::compiler_sdk::CompilerSdkMountData)->Result<()>{
    self.binding.validate()?;self.helper.validate()?;ensure!(self.version==1&&self.kind=="compiler_sdk_helper_terminal_failure_v1"&&&self.binding==expected&&&self.sdk==mount,"SDK original helper-failure ACK differs");Ok(())
}}
#[derive(Clone,Debug,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct FailureCloseRequest {pub version:u32,pub kind:String,pub binding:JobBinding,pub helper:HelperFailureData,pub cleanup_constraint:FinalizationConstraint}
impl FailureCloseRequest {pub(super) fn validate(&self)->Result<()>{self.binding.validate()?;self.helper.validate()?;
    ensure!(self.version==1&&self.kind=="compiler_sdk_failed_job_close_v1","SDK terminal-failure disposal format differs");Ok(())}}
#[derive(Clone,Debug,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct FailureCloseAck {pub version:u32,pub kind:String,pub request:FailureCloseRequest,pub original_helper_pid:u32,pub original_helper_start:String}
impl FailureCloseAck {pub(super) fn validate(&self,expected:&FailureCloseRequest)->Result<()>{self.request.validate()?;
    ensure!(self.version==1&&self.kind=="compiler_sdk_failed_job_resources_closed_v1"&&&self.request==expected&&self.original_helper_pid==expected.helper.original_helper_pid&&self.original_helper_start==expected.helper.original_helper_start,"SDK failed-job original closure ACK differs");Ok(())}}

#[cfg(test)]#[path="compiler_sdk_terminal_protocol_tests.rs"]mod terminal_tests;
