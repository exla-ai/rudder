//! Root-only SDK signer. Its key and original registration never leave Root.
use super::*;
use super::super::super::runtime_image::{compiler_qualification::CompilerQualificationScope,qualification::Envelope};
const SDK_REQUEST:&[u8]=b"rudder_compiler_sdk_qualification_ready_v1\0";
const SDK_RESPONSE:&[u8]=b"rudder_compiler_sdk_qualification_ack_v1\0";
impl RootRegistration {
    pub(in crate::root_storage_bootstrap) fn sign_original_compiler_qualification(&self,runtime:&RootRuntimeParent,
        export:&NativeExportData,scope:&CompilerQualificationScope)->Result<Envelope<serde_json::Value>> {
        // This reuses the genuine Native client/export and SAME original Root
        // ACK check, never a boolean/template/receipt constructor.
        scope.validate_config(&self.config)?;let base=self.sign_original_runtime_qualification(runtime,export)?;
        let sdk=runtime.compiler_qualification_ack_data()?;ensure!(sdk.mount.catalog==scope.sdk,"SDK fixed workload differs from original protected catalog");
        let mut body=serde_json::to_value(base.body)?;body["kind"]="compiler_sdk_qualification_ready".into();body["scope"]=serde_json::to_value(scope)?;body["sdk_proof"]=serde_json::to_value(sdk)?;body["sdk_proof"]["mount"].as_object_mut().unwrap().remove("catalog"); // scope already binds this exact catalog.
        let request=Envelope{mac:sign(&self.key.key.0,SDK_REQUEST,&body)?,body};ensure!(bytes(&request)?.len()<=qualification::LIMIT,"SDK signed request bound");
        self.qualification_original_registration_current()?;runtime.original()?.remaining()?;Ok(request)
    }
    pub(in crate::root_storage_bootstrap) fn verify_original_compiler_qualification_ack(&self,runtime:&RootRuntimeParent,export:&NativeExportData,
        scope:&CompilerQualificationScope,request:&Envelope<serde_json::Value>,ack:&Envelope<serde_json::Value>)->Result<()> {
        self.qualification_original_registration_current()?;scope.validate_config(&self.config)?;
        let registered=self.completed_runtime_registration()?;
        runtime.check_original_qualification_export(export,&registered,&self.config)?;
        let sdk=runtime.compiler_qualification_ack_data()?;
        let mut proof=serde_json::to_value(sdk)?;proof["mount"].as_object_mut().unwrap().remove("catalog");
        ensure!(request.body["sdk_proof"]==proof&&request.body["scope"]==serde_json::to_value(scope)?
            &&request.body["observation"]==export.observation&&request.body["registration_binding"]==serde_json::to_value(&registered.ack.binding)?
            &&request.body["root_nonce"]==self.nonce&&request.body["registration_proof_sha256"]==registered.ack.proof_sha256
            &&request.body["selected_executables"]==serde_json::to_value(runtime.selected_qualification_executables_data()?)?,"SDK request differs from genuine original owners/proof");
        verify(&self.key.key.0,SDK_REQUEST,&Signed{body:request.body.clone(),mac:request.mac.clone()})?;
        verify(&self.key.key.0,SDK_RESPONSE,&Signed{body:ack.body.clone(),mac:ack.mac.clone()})?;
        let body=ack.body.as_object().context("SDK original ACK body absent")?;
        let fields=["version","kind","qualification_id","qualification_nonce","request_sha256","registration_proof_sha256","original_enable_constraint","result","error"];
        ensure!(body.len()==fields.len()&&fields.iter().all(|f|body.contains_key(*f)),"SDK ACK exact schema differs");
        let a=&ack.body;let r=&request.body;
        ensure!(a["version"]==1&&a["kind"]=="compiler_sdk_qualification_ack"&&a["qualification_id"]==r["qualification_id"]&&a["qualification_nonce"]==r["qualification_nonce"]
            &&a["request_sha256"]==hex(&Sha256::digest(bytes(&request.body)?))&&a["registration_proof_sha256"]==r["registration_proof_sha256"]
            &&a["original_enable_constraint"]==r["observation"]["original_enable_constraint"]&&a["error"].is_null(),"SDK ACK not successful original request");
        let result=&a["result"];ensure!(result["scope"]=="immutable-compiler-sdk-role-only"&&result["state"]=="published"&&result["image"]==scope.sdk.cas_image
            &&result["inventory"]==serde_json::to_value(&scope.sdk.inventory)?&&result["manifest"]==serde_json::to_value(&scope.sdk.manifest)?&&result["installer_input"]==serde_json::to_value(&scope.sdk.installer_input)?,"SDK published semantic role differs");
        let expected=serde_json::json!({"format_version":1,"role":"immutable-compiler-sdk-role-v1","profile_id":scope.sdk.profile_id,"artifact_manifest":scope.sdk.artifact_manifest,"manifest":scope.sdk.manifest,"installer_input":scope.sdk.installer_input});
        ensure!(result["role_binding_sha256"]==hex(&Sha256::digest(bytes(&expected)?)),"SDK acknowledged a different semantic role");
        self.qualification_original_registration_current()?;runtime.original()?.remaining()?;Ok(())
    }
}
