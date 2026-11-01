//! Dedicated ordinary SDK compilation on the same authenticated engine lane.
//! No arbitrary command, project/lease constructor or context operation.
use super::*;
use rudder_engine::protocol::ObjectRef;
use super::super::sdk_job_protocol;
#[path="compiler_sdk_root_settlement.rs"]mod settlement;
#[path="compiler_sdk_terminal_reply.rs"]mod terminal_reply;
#[path="compiler_sdk_failed_root_settlement.rs"]mod failed_settlement;
#[derive(Clone,Serialize,Deserialize)]#[serde(deny_unknown_fields)]
pub(in crate::root_storage_bootstrap) struct BuildRequest {
    pub version:u32,pub kind:String,pub serial:u64,pub nonce:String,pub engine_instance_id:String,pub epoch:String,pub config_sha256:String,
    pub constraint:FinalizationConstraint,pub source_root:ObjectRef,pub input_topology:Option<ObjectRef>,pub action_key:ObjectRef,
    pub profile:rudder_engine::build::BuildProfile,
}
pub(in crate::root_storage_bootstrap) struct RootBuildTransfer {packet:RuntimePacket,request:Option<BuildRequest>,
    admitted:bool,terminal_ack:Option<sdk_job_protocol::TerminalFailureAck>,terminal_bytes:Option<Vec<u8>>,terminal_sent:bool,terminal_ready:bool,terminal_error:Option<anyhow::Error>,failed_disposal:Option<failed_settlement::FailedRootJobDisposal>,disposal:Option<settlement::RootJobDisposal>,process:Option<sdk_process::SdkProcess>,reply:Option<RuntimeEndpoint>,ack:Option<Vec<u8>>,sent:bool,failed:bool,error:Option<anyhow::Error>}
impl RootBuildTransfer {
    pub(in crate::root_storage_bootstrap) fn staged(packet:RuntimePacket)->Self{Self{packet,request:None,admitted:false,terminal_ack:None,terminal_bytes:None,terminal_sent:false,terminal_ready:false,terminal_error:None,failed_disposal:None,disposal:None,process:None,reply:None,ack:None,sent:false,failed:false,error:None}}
    pub(in crate::root_storage_bootstrap) fn poll_retaining(&mut self,owner:&RuntimeImageMountOwner,endpoint:&RuntimeEndpoint)->Result<bool>{
        if self.failed{return self.poll_terminal_reply_retaining(owner)}if self.sent{return self.poll_job_disposal_retaining(owner)}let result=self.run(owner,endpoint);if let Err(error)=result{self.failed=true;self.error=Some(error);return Ok(false)}result
    }
    pub(in crate::root_storage_bootstrap) fn admit_original_serial_retaining(&mut self,owner:&RuntimeImageMountOwner,high_watermark:&mut u64){
        let result=(||->Result<()>{ensure!(!self.admitted&&!self.failed,"SDK original admission cannot replay");
            self.request=Some(serde_json::from_slice(&self.packet.bytes)?);let request=self.request.as_ref().unwrap();let actual=&owner.admission.as_ref().context("actual Root registration absent")?.registration.ack;
            ensure!(self.packet.credentials.pid==owner.proof.engine.pid as i32&&self.packet.credentials.uid==1500&&self.packet.credentials.gid==1500&&self.packet.fds.len()==4
                &&request.version==2&&request.kind=="compiler_sdk_build_v2"&&request.engine_instance_id==actual.engine_instance_id&&request.epoch==actual.binding.epoch&&request.config_sha256==actual.binding.config_sha256,"SDK original serial scope differs");
            self.original_binding()?.validate()?;FinalizationBudget::from_constraint(&request.constraint)?.remaining()?;
            sdk_job_protocol::admit_serial_data(&self.original_binding()?,high_watermark)?;self.admitted=true;Ok(())
        })();if let Err(error)=result{self.failed=true;self.error=Some(error);}
    }
    pub(in crate::root_storage_bootstrap) fn nonce_data(&self)->Result<String>{let value:serde_json::Value=serde_json::from_slice(&self.packet.bytes)?;let nonce=value["nonce"].as_str().context("SDK original nonce absent")?;ensure!(uuid::Uuid::parse_str(nonce)?.to_string()==nonce,"SDK nonce is not canonical");Ok(nonce.into())}
    fn run(&mut self,owner:&RuntimeImageMountOwner,endpoint:&RuntimeEndpoint)->Result<bool>{
        ensure!(self.admitted,"original SDK serial admission absent");owner.current()?;let config=owner.config.as_ref().unwrap();ensure!(config.runtime_qualification.is_none()&&config.compiler_qualification.is_none(),"SDK fixed qualification refuses general compilation");
        let sdk=owner.compiler_sdk.as_ref().context("actual SDK mount absent")?;sdk.current()?;
        if self.request.is_none(){self.request=Some(serde_json::from_slice(&self.packet.bytes)?);}
        let request=self.request.as_ref().unwrap();let a=&owner.admission.as_ref().unwrap().registration.ack;
        ensure!(request.version==2&&request.kind=="compiler_sdk_build_v2"&&request.engine_instance_id==a.engine_instance_id&&request.epoch==a.binding.epoch&&request.config_sha256==a.binding.config_sha256
            &&self.packet.credentials.pid==owner.proof.engine.pid as i32&&self.packet.credentials.uid==1500&&self.packet.credentials.gid==1500&&self.packet.fds.len()==4,"SDK original build request differs");
        if self.reply.is_none(){self.reply=Some(RuntimeEndpoint::capture(self.packet.fds[3].try_clone()?)?);}
        self.nonce_data()?;let original=FinalizationBudget::from_constraint(&request.constraint)?;original.remaining()?;check_cancel(&self.packet.fds[2])?;
        request.source_root.validate()?;ensure!(request.source_root.kind==rudder_engine::protocol::ObjectKind::SourceRoot,"SDK selected source kind differs");request.profile.validate()?;
        ensure!(request.profile.policy.cacheable()&&!request.profile.targets.is_empty(),"SDK compiler refuses secret/nonhermetic effects");
        for (i,mode)in[(0,None),(1,Some(0o700))]{let m=File::from(self.packet.fds[i].try_clone()?).metadata()?;
            ensure!(m.is_dir()&&m.uid()==1500&&m.gid()==1500&&mode.is_none_or(|mode|m.mode()&0o7777==mode),"SDK original source/output directory differs");}
        if self.process.is_none(){sdk_process::SdkProcess::capture_retaining(&mut self.process,sdk.proof.clone(),sdk.admission.clone(),original.clone(),sdk.root.as_ref().unwrap().try_clone()?,sdk.data.as_ref().unwrap().clone(),
            Some(serde_json::to_value(request)?),Some((self.packet.fds[0].try_clone()?,self.packet.fds[1].try_clone()?)),Some(self.packet.fds[2].try_clone()?))?;
            self.process.as_mut().unwrap().start_retaining()?;
        }
        if !self.process.as_mut().unwrap().poll_retaining()?{return Ok(false)}
        let report=self.process.as_ref().unwrap().result_data()?;original.remaining()?;check_cancel(&self.packet.fds[2])?;sdk.current()?;
        if self.ack.is_none(){self.ack=Some(canonical_json(&serde_json::json!({"version":2,"kind":"compiler_sdk_build_ack_v2","serial":request.serial,"nonce":request.nonce,"action_key":request.action_key,
            "source_root":request.source_root,"sdk":sdk.data,"result":report,"child_exit_code":0,"stdout_eof":true,"stderr_eof":true}))?);}
        if !self.reply.as_ref().unwrap().send_data(self.ack.as_ref().unwrap(),&[])?{return Ok(false)}self.sent=true;original.remaining()?;Ok(true)
    }
}
pub(in crate::root_storage_bootstrap) fn is_request(bytes:&[u8])->Result<bool>{let value:serde_json::Value=serde_json::from_slice(bytes)?;Ok(value["kind"]=="compiler_sdk_build_v2"||value["kind"]=="compiler_sdk_build_v1")}
