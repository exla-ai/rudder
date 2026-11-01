//! Dedicated SDK qualification on the existing original Root/authenticated
//! relay. Runtime's frozen scope/domain is not SDK evidence or permission.
use super::*;
use rudder_engine::protocol::ObjectRef;
use super::qualification::{RuntimeQualificationScope,NativeExportData,Envelope};
use rudder_engine::execution::compiler_sdk::CompilerSdkCatalogData;
#[derive(Clone,Debug,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompilerQualificationScope {
    pub format_version:u32,pub scope:String,pub qualification_id:String,pub qualification_nonce:String,
    pub image:String,pub image_receipt_sha256:String,pub source_manifest_sha256:String,
    pub inventory:ObjectRef,pub manifest:ObjectRef,pub installer_input:ObjectRef,
    pub native_sha256:String,pub buddy_sha256:String,pub service_sha256:String,pub launcher_sha256:String,
    pub sdk:CompilerSdkCatalogData,
}
impl CompilerQualificationScope {
    pub(in crate::root_storage_bootstrap) fn runtime_scope(&self)->Result<RuntimeQualificationScope>{
        let mut data=serde_json::to_value(self)?;let map=data.as_object_mut().unwrap();map.remove("sdk");map.insert("scope".into(),"disposable-runtime-floor-and-original-client-only".into());Ok(serde_json::from_value(data)?)
    }
    pub(in crate::root_storage_bootstrap) fn validate_config(&self,config:&HostIngressConfig)->Result<()> {
        ensure!(self.format_version==1&&self.scope=="disposable-fixed-compiler-sdk-only","SDK qualification fixed scope differs");self.sdk.validate_data()?;
        let runtime=self.runtime_scope()?;runtime.validate_config(config)?;
        ensure!(config.compiler_sdk.as_ref()==Some(&self.sdk)&&config.runtime_qualification.as_ref()==Some(&runtime),"SDK qualification not bound to original disposable runtime purpose");Ok(())
    }
}
impl RootRuntimeParent {
    pub(in crate::root_storage_bootstrap) fn compiler_qualification_ack_data(&self)->Result<&compiler_sdk::CompilerSdkEnableData>{
        self.original()?.remaining()?;ensure!(self.ready&&!self.failed,"actual original SDK enable ACK absent");
        let data=self.accepted_ack.as_ref().context("SDK original runtime ACK absent")?.compiler_sdk.as_ref().context("SDK original compiler ACK absent")?;
        data.mount.validate_data()?;let probe=data.probe.as_ref().context("SDK fixed workload original result absent")?;
        ensure!(probe.original_constraint==self.original()?.constraint(),"SDK workload replaced original enable clock");compiler_sdk::validate_workload(&probe.workload,&probe.original_constraint)?;Ok(data)
    }
}
pub(in crate::root_storage_bootstrap) struct RootCompilerQualification {
    scope:CompilerQualificationScope,request:Option<Envelope<serde_json::Value>>,wire:Option<Vec<u8>>,sent:bool,
    pending:Option<storage::Packet>,ack:Option<Envelope<serde_json::Value>>,published:bool,
}
impl RootCompilerQualification {
    pub(in crate::root_storage_bootstrap) fn staged(scope:CompilerQualificationScope)->Self{Self{scope,request:None,wire:None,sent:false,pending:None,ack:None,published:false}}
    pub(in crate::root_storage_bootstrap) fn poll_retaining(&mut self,runtime:&RootRuntimeParent,registration:&mut engine_registration::RootRegistration,
        host:&mut RootHostControl,export:&NativeExportData)->Result<bool>{
        if self.published{return Ok(true)}runtime.original()?.remaining()?;registration.qualification_original_registration_current()?;
        self.scope.validate_config(host.registration_config())?;ensure!(export.scope==self.scope.runtime_scope()?,"SDK Native observation belongs to another original purpose");
        if self.request.is_none(){self.request=Some(registration.sign_original_compiler_qualification(runtime,export,&self.scope)?);
            self.wire=Some(canonical_json(&serde_json::json!({"kind":"runtime_image_qualification_ready_v1","rpc_id":export.observation["request_nonce"],"packet":self.request}))?);
            ensure!(self.wire.as_ref().unwrap().len()<=qualification::LIMIT,"SDK relay exceeds fixed bound");}
        if !self.sent{runtime.original()?.remaining()?;if !host.registration_send(self.wire.as_ref().unwrap())?{return Ok(false)}self.sent=true;runtime.original()?.remaining()?;}
        if self.pending.is_none(){host.runtime_qualification_receive_retaining(&mut self.pending)?;}
        let Some(packet)=self.pending.as_ref()else{return Ok(false)};
        #[derive(Deserialize)]#[serde(deny_unknown_fields)]struct Relay{kind:String,rpc_id:String,packet:Envelope<serde_json::Value>}
        let reply:Relay=serde_json::from_slice(&packet.bytes)?;self.ack=Some(reply.packet);
        ensure!(reply.kind=="runtime_image_qualification_ack_v1"&&export.observation["request_nonce"]==reply.rpc_id,"SDK original ACK correlation differs");
        registration.verify_original_compiler_qualification_ack(runtime,export,&self.scope,self.request.as_ref().unwrap(),self.ack.as_ref().unwrap())?;
        runtime.original()?.remaining()?;self.published=true;Ok(true)
    }
}
